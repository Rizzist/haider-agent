# F3 — compaction concurrency contract — mutation notes

Implementer: Fable 5 (run orchestrator directly, not a lane). Branch
`f3-compaction-contract` @ `69b92ca`. Owner contract, verbatim: "we can
switch while the compaction is happening? and queue messages (even queue
steered but doesnt send until its done, that way we have proper blocking
until its done, and we probably need a compacting state for the session as
well) do this".

## Probed truth before any fix

The three laws were written FIRST as desired behavior and run against
unmodified code (FakeStep::Delay{1500ms} holds the manual-compaction window
open; the world polls the journal for `RunState::Compacting` before acting):

| Law | Verdict on unmodified code |
|---|---|
| `submit_during_manual_compaction_queues_and_runs_after` | PASSED — admission marks Queued (Compacting is non-terminal), the supervisor channel buffers while the Compact arm awaits, and the queued turn starts strictly after the compaction terminal (journal-seq ordered) |
| `steer_during_manual_compaction_blocks_and_never_reaches_the_summarizer` | PASSED — rpc `turn_submit` always mints a fresh run id (even for Steer), so a composer steer can never latch onto the compaction run; the text is absent from the summarization request and delivers in the first post-compaction request |
| `switch_during_manual_compaction_lands_after_it` | FAILED — REAL BUG. The mid-window `model_selected` fact advanced the journal head; the compaction commit's head CAS refused Busy ("session history advanced"), `append_failure` latched the compaction run `Errored`, and the retry died on the terminal gate ("worker run … is already terminal"). A benign metadata fact permanently wedged the compaction. |

`RunState::Compacting` already existed, was already published by manual
compaction (`worker.rs:2788`), and already rendered as the `⊟ COMPACTING`
badge — the state half of the owner's ask was present; these laws make it
load-bearing.

## Fix

Actor `WorkerAppend` CAS (`session_hub/actor.rs`): on `head != expected_head`,
classify the delta `(expected_head, head]` with a bounded store read
(`MAX_TOLERATED_DELTA = 64`). A delta made ONLY of session-config facts
(`SessionConfigEventPayload`, internally tagged — foreign payloads cannot
parse as it) is journal movement with NO conversation-tree movement, so the
compaction node's planned parent is still the tree head and the batch
commits. Any structural payload, read failure, coverage gap, or oversized
delta keeps the honest Busy rejection.

Diagnosis detour worth recording: the law initially still failed with
`expected_head=20, head=19` — the actor's head BEHIND durable truth. Root
cause: the test selected via `store.select_session_model` directly,
bypassing the actor's SelectModel arm that advances the in-memory head — an
impossible client. `SessionHub::select_session_model` was widened
`pub(crate)` (the `accept_internal_turn` convention) and the law now selects
through the actor, like the wire does. The tolerance remains necessary for
the REAL race (a select landing between the compactor's `latest_seq`
capture and the CAS) and, without it, that race wedges the compaction
permanently via the Errored latch — the failure mode the probe demonstrated.

## Executed runtime kills

All executed post-commit against `69b92ca`: apply, run the named test
("running 1 test" observed), record the failure, revert, re-run green.

| # | Mutation (seam) | Law | Observed kill |
|---|---|---|---|
| FM1 | Tolerance reverted: CAS mismatch unconditionally Busy (`if true`) | `worker_head_cas_tolerates_a_config_fact_delta` | KILLED — the config-fact delta batch is refused Busy; the pin's `expect("a config-fact-only delta must not reject the batch")` fails |
| FM2 | Classifier accepts every payload (`.all(\|_\| true)`) | `worker_head_cas_rejects_a_compaction_batch_after_history_advances` (pre-existing pin, unchanged) | KILLED — the stale batch behind a RUN-STATE advance commits; the reject pin's `expect_err` fails. The two pins are a complementary pair: one kills the missing tolerance, the other kills an over-permissive classifier |

Honest note: `MAX_TOLERATED_DELTA` (the 64-envelope scan bound) is a safety
valve with no observing law — staging a 65-fact interleave is not a
realistic failure, and the bound degrades to the pre-F3 Busy behavior, never
to silent acceptance.

## Not in this wave

- Auto (mid-turn) compaction steer isolation: structurally safe — the
  summarizer (`DaemonContextCompactor::compact`) assembles its request from
  `plan_compaction` history only; harness nudges are consumed by
  conversational request assembly. No law stages the timing (brittle);
  revisit if the compactor ever consumes pending input.
- TUI composer hint ("queued — compacting") rides after the F2 merge; the
  `⊟ COMPACTING` badge already renders.

## Gate

`cargo fmt --all -- --check` clean at commit; 11/11 pair_switch +
worker_head_cas green post-revert. Ledger 1645 → 1649 (+4 new tests: three
F3 runtime laws + the tolerance pin; the commit message's "5 new laws;
1650" over-counted by including the pre-existing reject pin — 1649 in
`test-baseline.txt` is the truth).
