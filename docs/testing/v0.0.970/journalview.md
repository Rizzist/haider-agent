# journalview — durable narrative and compaction announcements

Lane `lane-970-journalview`, v0.0.970, gpt-6-astra. No benchmark score or latency improvement is claimed.

## Claims audit and scope

The supplied lane files and both turnperf evidence rounds were read first. Their historical file/line references are **drifted**, not current-tree coordinates. The relevant constructs were located again by name.

- **Correct, with an important distinction:** the root-cause investigation at `/Users/rizzist/.claude/jobs/1a0b5b6c/tmp/971-benchmark-rootcause.log` identified existing capture. `RunJson.events` already serializes the lossless `HeadlessRunEvents` ledger; `ItemEvent` already carries assistant text, emitted reasoning summaries, tool calls and results. This lane declares those existing capture points and adds durable physical-request correlation. It does not claim that raw capture was absent.
- **Correct, with an announcement nuance:** `DaemonContextCompactor::run_compaction` already appended the `ContextCompaction` lifecycle and `NodeCommitted(NodeKind::Compaction)` together. Their scope and artifact were durable and observable through raw envelopes. Missing were a normalized countable announcement, actual dropped/retained counts, and explicit trigger-turn/request identity. The memwindow investigation is `/Users/rizzist/.claude/jobs/1a0b5b6c/tmp/970-memwindow-investigate.log`.
- **Correct:** turnid's `ProviderRequestAttemptV1` supplies the exact session/run/turn/request coordinates used by `X-Haider-Turn`. Request ordinals are physical attempts, including Side summarization and failed attempts; they are not derived from output adjacency.
- **Supplied benchmark premise:** compaction never fired in the external benchmark because aggregate input stayed below the soft threshold at `--speed normal`. The prior investigation supports this by source/aggregate-input inference; exact benchmark journals were unavailable to this lane. Under that premise, silence did **not** cause the benchmark's 0/20. These announcement changes matter for AHRB's forced-compaction experiment.

The turnperf lenses require retaining persist-before-publish, exact raw replay, existing request barriers, coalesced primary deltas, and the JSONL path without retained duplicate summaries. This lane makes no timing claim and moves no primary-request durability boundary. Private compactor output, previously discarded on failure, now has durable item lifecycles.

Current citation audit (historical coordinates are drifted; the constructs below
were located by name in the final lane source):

| Historical claim | Audit and current source |
|---|---|
| Raw narrative capture was absent | **Wrong premise**: existing `RunJson.events` retains raw envelopes, `crates/haider-cli/src/run.rs:2002`. Declaration and physical-request correlation were missing. |
| JSON and replay need one normalized view | **Correct**: both use the same reducer at `crates/haider-cli/src/run.rs:1251` and `:2047`; implementation is `crates/haider-client/src/provider_rounds.rs:47`. |
| turnid provides reusable physical request identity | **Correct, drifted coordinates**: activated only after admission at `crates/haider-core/src/actor.rs:4213`; durable envelope stamping at `:11148`; shared-leaf metadata insertion at `crates/haider-protocol/src/envelope.rs:119`. |
| Recovery must retain the source request | **Correct, drifted coordinates**: checkpoint item lookup at `crates/haider-core/src/actor.rs:7782`; incoming rebind failure cleanup at `:3540`, `:3558`, and `:3607`. |
| Compaction already persists scope/artifact | **Correct, drifted coordinates**: announcement built at `crates/haider-daemon/src/worker.rs:2415`, committed with the existing item/node at `:2445`. |
| Counts must describe the actual dropped projection | **Correct**: counted before source-history expansion at `crates/haider-daemon/src/worker.rs:1500`; matching atomic overlay enforced at `crates/haider-store/src/event_store.rs:22006`. |
| Private summarizer output needs durable capture | **Correct**: append helper at `crates/haider-daemon/src/worker.rs:620`; unsuccessful Finish now preserves incomplete text. |

The external checker citations were also re-audited in the read-only
`harness-bench` tree: source raw narrative at `src/wave4.rs:45`, duplicate-delta
handling at `:317`, metadata checks at `:480`, marker count/span comparison at
`:658`, announced-only scoring at `:770`, and injected `/actor` at
`src/driver.rs:2544`. These support the declared external limitation below.

## Delivered surface

Existing durable `item` Started/Delta/Completed events carry additive `payload.provider_request`: session_id, run_id, turn_ordinal, request_ordinal, request_kind. The same coordinates accompany actor-generated request facts, tool calls/results and states. `provider_finish_reason` is included only after that Finish was actually emitted; early narrative completed at a tool-call boundary cannot predict it. A
prompt-omitted atomic Started/Completed `provider_round_terminal_v1` pair preserves Finish when
post-stream facts commit separately; the existing atomic final-text suffix
retains its finish reason without an extra append. The marker follows usage in
both budget paths, so accounting and delta metadata stay budget-independent.
Provider timing ends before this local journal work. Recovery finds the exact checkpoint item's durable coordinates before settling tool/child results, even if a later Side request exists.

The envelope already provides `seq`, `schema_version=1`, and `committed_at_ms`. Correlation is inserted into the raw payload skeleton before append, retaining shared reply leaves. No live serializer decorates a durable event. Existing journals without these fields remain readable.

Both `--output json` and `haider run --replay` expose `provider_rounds`, reduced by the same client implementation. Each round has request coordinates, emitted_text, reasoning_summary, tool_calls, results, terminal_cause. Narrative entries contain item_id, text, completed, first_seq, last_seq, committed_at_ms, schema_version. Deltas assemble through a shared reply arena. Completed snapshots do not duplicate deltas or replayed prefixes from an earlier physical request. Unavailable reasoning is an empty array; a cause absent from the journal is null. No coordinates are invented for old journals.

The private summarizer uses the same narrative lifecycle with `provider_purpose="compaction"`, Side request identity, actual finish reasons, and `provider_terminal_cause` for unsuccessful attempts. Failed text stays incomplete and already-emitted reasoning survives. These records are prompt-omitted and cannot replace the user's final response in either live JSON or replay.

`context_compaction` commits in the same `append_at_head` batch as the overlay item/node and precedes the resumed/terminal state. Its fields are:

| Field | Meaning |
|---|---|
| turn_ordinal / request_ordinal | Trigger run's durable turn and successful summarization request; automatic compaction stays in the triggering run, manual idle compaction owns its own run. |
| covers_from / covers_to | Inclusive original history ancestry covered by the new overlay. |
| dropped_item_count / dropped_item_unit | Actual active provider-message prefix replaced; unit is explicitly `provider_message`. |
| retained_suffix_size / retained_suffix_unit | Original provider-message suffix retained verbatim, excluding new summary and request-only scaffolding. |
| summary_artifact | Artifact used by the committed compaction node. |
| operation_id / resume_cause | Existing compaction identity and automatic/manual cause. |

These units are intentionally explicit. Node count and provider-message count can differ. A replacement compaction rereads the original covered source but drops the active old summary only once. Journal history itself is never deleted. Failed or rejected compaction commits emit no announcement.

Replayparity remains **zero normalized fields inside RawEnvelope**, including metadata, scope, timestamps and serialized encoding. `provider_rounds` is derived container metadata. Prompt replay ignores additive metadata and prompt-omitted summary/announcement records. See `docs/event-schema-changelog.md` for the additive schema ledger and compatibility policy.

## Adapter declaration

No product manifest exists at `bench/adapters/haider-agent/adapter.toml`. The only manifest is read-only at `/Users/rizzist/Documents/CODING/harness-bench/adapters/haider-agent/manifest.toml`; it was not modified. The exact owner-paste TOML is [journalview-adapter.toml](journalview-adapter.toml). Replace its existing metadata table and append the declared narrative/compaction tables and rules, following the fragment's comments.

The fragment declares both assistant-text and reasoning delta capture, timestamp/schema metadata, and the compaction announcement. AHRB injects `/actor` from the submitted marker before normalization; this is checker correlation, distinct from product-durable `provider_request`. Metadata pointers address the retained `_ahrb_source_raw` because the metadata checker consumes NormalizedEvent, while narrative pointers address the source envelope directly.

**Current AHRB scoped-checker limitation:** its omitted-count/span checks expect synthetic `AHRB-HISTORY-*` ordinary assistant markers, not product graph-node IDs or provider-message counts (`harness-bench/src/wave4.rs`, `src/runner.rs`). Mapping these incompatible units by assertion would create an inaccurate declaration. The exact fragment therefore declares announced-only support, scoring at most 0.5 under the current row-73 checker, not a claimed row-73 pass. The benchmark owner must map Haider's truthful scope to its synthetic history evidence before declaring scoped credit. Row 69 also remains externally undeclared until the owner pastes the fragment.

## Verification

Named behavior pins:

- `narrative_items_keep_request_correlation_and_exact_live_journal_parity`: two requests, eight split text/reasoning deltas, completed snapshots, tool result and exact full raw stream/journal equality.
- `recovered_child_settlement_keeps_its_source_request_before_next_provider_round`: original request 2, intervening Side request 4, recovered result still request 2, next request 5, no child redispatch and raw parity.
- `journalview_json_and_replay_pin_both_narrative_sides_per_request`: real CLI JSON, tool boundary, intermediate/final text and reasoning, metadata, results, finish cause, exact JSON/replay event and round equality.
- `journalview_retry_completion_does_not_duplicate_an_earlier_requests_text`, `journalview_retry_cancellation_and_stream_failure_have_honest_causes`: replayed-prefix completion, partial reasoning and retry terminal precedence.
- `journalview_admission_refusal_never_invents_a_physical_request`: refusal before the first request and after a completed tool round cannot expose a reserved, unsent ordinal; prior narrative ownership and Finish remain intact.
- `journalview_rebind_failure_closes_recovered_items_under_the_source_request`: recovered text, reasoning, and a partial tool close before a rebind-error terminal; every event retains original request 2, no Finish or request 3 is invented, no provider request is sent, and live/durable suffixes match exactly.
- `journalview_unsuccessful_summary_finish_keeps_text_incomplete`: all six non-EndTurn Finish variants retain partial text with `completed=false` and their exact terminal cause.
- `journalview_large_delta_projection_uses_shared_arena_and_replays_identically`, `journalview_legacy_and_unknown_events_preserve_raw_without_invented_rounds`: arena assembly and forward/legacy behavior.
- `journalview_private_summary_does_not_replace_the_live_response`, `journalview_replay_private_summary_never_becomes_final_response`: private compactor output cannot leak into final response.
- `automatic_compaction_plans_and_commits_on_the_accepted_branch`: actual one-node/one-message drop and retained suffix 5; branch, node, artifact and stream scope agree.
- `cm1f_manual_compaction_usage_is_journaled_once_in_its_own_lane`: forced initial two-node/two-message drop with empty suffix, replacement drops one active summary, failed followup preserves narrative but emits no announcement. All six non-EndTurn Finish variants retain incomplete text, emitted reasoning, and the exact Finish; none commits an overlay or announcement.
- `worker_compaction_fact_requires_typed_scope_matching_overlay_and_active_run`: ten malformed announcement mutations rejected (missing field, unknown kind, wrong unit/scope, absent run, wrong branch, non-durable, prompt-visible, zero coordinate, missing overlay).
- `journalview_additions_are_documented_without_a_schema_bump` and the existing exhaustive schema-changelog inventory.

Scoped verification on the providerrebind content merge passed: all 91 runtime
tests, the three existing core journalview tests, the added rebind regression,
four daemon context/compaction tests, ten turn-hygiene tests, the one-shot JSONL
golden, and the instruct-pipe byte pin. The runtime scope ran before the final
rebind cleanup; the full workspace gate below covers the complete final source.
The merged instruct-pipe value is **13,552 -> 13,552 bytes**; no prompt/tool
surface changed in this lane. The authoritative `xtask test-count --update`
recount is **4,910 merged upstream -> 4,925** source test markers.

All JSONL goldens were regenerated through their repository update flags and
then exercised without those flags by the full workspace gate. The line review
accounts for **70 changed/added lines**: text 15, tool 40, one-shot 15. These
contain additive correlation/Finish metadata, four atomic terminal marker pairs,
their sequence/event-ID shifts, and the resulting item-allocation shifts.
Every other semantic field matches the original golden. The tooling-regenerated
`provider_request_no_budget.json` remains byte-identical to merged upstream.
See [golden review](journalview-evidence/golden-review.txt), its retained checking
script, and the scoped logs in [journalview-evidence](journalview-evidence/).

## Continuation and independent verification

The interrupted run's edits were retained. The original brief was recovered from
`/Users/rizzist/.claude/jobs/1a0b5b6c/tmp/970-journalview-brief.md`; it requires
the full workspace tests and Clippy test targets, and does not require an
additional code-mutation experiment. Current owner/common instructions override
its obsolete instruction to commit: all work remains uncommitted.

Before the final gate, `git fetch origin wave-970 && git merge --no-commit
origin/wave-970` failed to write `FETCH_HEAD` outside the sandbox; the required
fallback `git merge --no-commit origin/wave-970` then failed to create
`ORIG_HEAD.lock`. Initially HEAD and the fallback ref were both `372a2639`.
The final-gate ref check caught casstream landing while tests rebuilt, advancing
`origin/wave-970` to `73fe3f68f71b6daffffbb330beb9fabd27141cb7`, and stopped the
gate before any stale-tree verdict.

The fetch/merge commands were attempted again with the same sandbox failure.
Their content merge was completed in the writable working tree using Git's
three-way `merge-file -p` against `372a2639`, the lane content, and `73fe3f68`.
All 89 incoming files were preserved; overlapping `headless.rs` and
`event_store.rs` merged cleanly with both changes intact. `test-baseline.txt`
was reserved for an authoritative xtask recount, never hand-merged. The exact
merge manifest and backups are recorded in
`/tmp/journalview-cont-merge-manifest.json`. No Git index/ref or merge commit was
written; HEAD remains `372a2639`. **The content merge is resolved in the working
tree; the orchestrator must record the merge.** That gate required `73fe3f68`
still be current; the subsequent providerrebind merge is documented below.

Two independent verifiers found two real issues, both corrected before the final
gate and both accepted as SHIP on re-review:

1. **Phantom request after admission refusal.** Correlation had activated when
   an ordinal was reserved, before the budget guard could refuse transport.
   It now activates immediately before the atomic attempt append, after
   admission and pending-delta cleanup; append failure restores the previous
   owner and Finish. A regression covers first-request and later-request refusal.
2. **Unsuccessful summary marked complete.** A non-EndTurn Finish closed text
   as a successful AgentMessage. It now uses IncompleteAgentMessage while
   retaining the exact provider Finish. Daemon and projection regressions cover
   Cancelled, Error, MaxTokens, Refusal, ToolUse, and PauseTurn.

The next full workspace gate exposed stale CLI payload expectations and runtime
sequence pins. While reviewing the runtime lifecycle failures, the narrative
verifier identified a third real issue: a completion-only terminal marker broke
the frozen Started/Completed writer contract. Both primary and empty-summary
terminal markers now use an atomic Started/Completed pair. The runtime helper
still requires every Completed item to have an open Started item, and additionally
validates exact terminal correlation, reason, uniqueness and render policy.
The empty-summary fixture pins the pair's adjacency, identity and metadata.
CLI terminal payloads and the full runtime sequence now assert every additive
field, without stripping metadata or reducing the equality checks.

The corrected tree's next full run passed the CLI and runtime suites but hit
`oauth::tests::device_flow_runner_continues_and_honors_slow_down_interval` at
`oauth_tests.rs:1262` (no device-start response within its existing drive loop).
Both protected OAuth files are unchanged. The exact workspace-built test binary
then passed that test in isolation (0.37 s) and its entire daemon suite with four
test threads (1,057 passed, zero failed, three existing ignores). The final full
gate therefore uses `RUST_TEST_THREADS=4`, matching the prior casstream gate's
shared-host scheduling. No test, assertion, deadline, or platform gate was removed
or relaxed. Both failed full-run logs and the unchanged diagnostic reruns are
retained in `journalview-evidence/first-gate/` and `second-gate/`.

Verifier accounting at this stage: **findings=3, real=3, noise=0**. Research identified evidence
documentation omissions separately; those are not counted as verifier findings.

## Providerrebind merge-forward continuation

The full four-thread workspace test, Clippy, formatting, unsafe, QA and xtask
checks passed on the content merged through `73fe3f68`. The closing ref guard
then observed `origin/wave-970` advance to
`38359fd3ba799c3e32a09c414f6f41abb90442bd` (providerrebind), withholding a stale-tree
verdict. That green run is retained in `journalview-evidence/green-73fe3f68/`.

Fetch and Git merge recording remain blocked by the same writable-directory
boundary. The next content merge used `73fe3f68` as its base and preserved all
129 incoming files. Actor, runtime tests, worker, schema inventory and store
overlaps merged cleanly. The only textual conflict was the additive changelog;
both complete sections were retained. The new manifest is
`/tmp/journalview-providerrebind-merge-manifest.json`. The orchestrator still
owns recording the resolved Git merge; HEAD remains `372a2639`.

The merge verifier found a fourth real issue: incoming rebind validation and
rotation-append failures used the bare error-terminal helper even when recovery
or reconnect had restored open items. The three exits now use the existing
item-cleanup error path, keeping those items under their source request and
closing them before the terminal. This is the explicitly identified minimal
change to providerrebind's landed block in `actor.rs`; no protected OAuth file
was changed. A new route-checkpoint regression covers cleanup, original request
coordinates and no new provider send. The exact regression passed, and the
independent narrative verifier re-reviewed all three exits and its assertions,
returning SHIP with no additional findings. The compaction verifier separately
re-reviewed the merged worker/store/schema overlaps and adapter declaration,
also returning SHIP with no new findings. Final verifier accounting is
**findings=4, real=4, noise=0**; the independent review does not replace the
merged full gate below.

## Final merged gate

**PASS**, 2026-09-05, macOS arm64, content merged through
`38359fd3ba799c3e32a09c414f6f41abb90442bd`. The final guard confirmed that ref
was still current. All 76 recorded source hashes remained unchanged through
the gate. Both protected OAuth files are byte-identical to merged upstream.

- `cargo test -q --workspace --locked --no-fail-fast`: exit 0, with four test
  threads. The full runtime suite passed (91), the core library passed with the
  added regression, and the daemon library passed (1,069 passed, zero failed,
  three existing ignores). The long TUI 10k-to-200k-row suite passed in 287.93 s;
  this unoptimized run is correctness evidence, not a release timing claim.
- `cargo clippy --workspace --tests --locked -- -D warnings`: exit 0.
- `cargo run -q --locked -p xtask -- check`: exit 0; baseline 4,925 matches.
  Nine file-length soft-cap warnings remain nonfatal.
- Formatting, whitespace, unsafe-count checks and all 65 QA self-tests: exit 0.
  The exact rebind regression also passed as a precheck.

No test assertion, deadline, ignore, or platform gate was weakened. The built
`haiderd` exceeds 10 MiB; disk stayed above the required 700 MiB floor. Goldens
passed under normal test execution after tooling regeneration and line review.

Raw logs, exact environment, merge manifests, source hashes, golden review and
verifier accounting are indexed in
[journalview-evidence/README.md](journalview-evidence/README.md), with machine-readable
status in [result.json](journalview-evidence/result.json). Raw test-result totals
include nested fixture subprocesses and are explicitly labeled in
`workspace-summary.json`; they are not the distinct source-marker recount.

**Handoff:** all work remains uncommitted. The orchestrator must record the
resolved content merge. The external benchmark owner must paste
`journalview-adapter.toml`; its compaction declaration is announced-only, with
scoped credit still requiring the checker-unit mapping described above.

## CI error registry walk

Applied `scripts/qa-gate/CI_REGISTRY_WALK_QAGATE3.md`: #19 formatting/diff/clippy; #20 test recount; #22–23 correlation metadata remains content-free; #29 persist-before-publish and source request on recovery; #30 named live/journal/JSON/replay pins; #33 additive optional fields and schema ledger; #38 shared reply arena, no extra retained JSONL summary; #41/#42/#44/#64/#71/#72/#74 hermetic profiles, real UDS and prebuilt siblings with haiderd >10 MiB; #73 tooling-generated golden review; #77 no unsafe; #94/#95 no new product deadlines or external waits; #96 no performance claim from historical estimates. Other registry surfaces are unaffected. Linux/Windows behavior is by inspection; execution is macOS only.
