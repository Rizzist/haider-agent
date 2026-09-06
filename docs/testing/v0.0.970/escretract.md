# escretract — v0.0.970 daemon/contract evidence

Branch: `lane-970-escretract`. Starting head:
`620fc1ce2faf0344c2d27c172fabe2c2f99bd253`. Final merge-forward target:
`022ccde3583f5eb3807d084ae69ab33b7af68b16` (`origin/wave-970` sampled
before the final full test pass).

The required fetch and no-commit merge were attempted. Fetch cannot write
`FETCH_HEAD` and merge cannot write `ORIG_HEAD` in the read-only worktree Git
metadata. The initial local upstream ref matched HEAD; it advanced during
verification. Its exact binary diff was checked and applied to the working
files with `git apply --check` / `git apply`, preserving both sides without
conflicts. The resulting source includes the upstream platform/test fixes and
all upstream evidence. Git HEAD itself remains unchanged; the orchestrator
must record the merge. All changes are uncommitted. The supplied LANE-COMMON,
LANE-BRIEF and turnperf/turnperf2 files remain untouched.

## Implemented behavior

- Feature-gated `turn.retract` and `haider session retract --session <id>`.
  RPC and CLI return the exact accepted draft with complete attachment blocks,
  original prompt sequence and retraction sequence. The reusable client pins
  the run/generation and both receipt identities through reconnect and the
  typed `too_late` fallback to ordinary cancel. SIGINT is unchanged.
- The same cancellation transaction helper owns ordinary cancel and retract.
  Retract commits Cancelling, `prompt_retracted` and its response receipt in
  one transaction, then publishes all facts and wakes the existing supervisor.
  No journal row or CAS attachment is deleted. Terminal stamping retains
  `reason: retracted` on Cancelled, including restart cleanup.
- A prompt-omitted first-response boundary precedes delta coalescing and
  response items. The store serializes it with retraction, converting a losing
  response to `response_delta_discarded` with the original normalized delta.
  A bounded snapshot of the already-buffered cancellation tail covers a
  ready-delta/cancellation tie. Empty deltas, finish, usage and network metadata
  do not close the editing window. Existing request reservations and usage
  release/accounting are preserved.
- Provider-history compilation omits the exact retracted user sequence in
  cold, warm, compacted and evicted caches. TUI display removes the exact
  committed node and adjusts its indexes. Fork pickers hide the prompt;
  explicit hidden-node fork cuts are refused and later forks remap retained
  retraction cursors when the copied journal is renumbered.
- Markdown, JSON, native pipe and foreign transcript exports hide the prompt.
  Native sidecars rebuild a new generation on retraction, including cold and
  crash reconciliation. Raw JSONL and replay retain every original envelope
  and the new facts. Incremental display consumers apply the retraction fact;
  raw replay remains the append-only automation authority.
- Contract and schema additions are documented with schema version 1. This
  lane provides daemon/contract support and transcript projections; keyboard
  binding/composer application of the returned draft is the client UI half.

## Citation audit and territory

Read LANE-COMMON first, then the brief, JSONL/automation/schema contracts and
both rounds of supplied turnperf evidence. Only relevant constructs were relied
on; no timing estimate from a historical lens is claimed as a measurement here.

| Supplied evidence | Audit |
| --- | --- |
| Automation `turn.cancel` frame citations 3434/4484 | Drifted: current `frame.rs` request/response at 3768/4931 before final formatting. Same control authority and receipt semantics. |
| Round-2 D3 first-byte cites 2247/2328 | Drifted: `openai.rs:2370` emits the optional trace on transport bytes before SSE semantic decode; unsuitable as durable response authority. |
| Round-2 D3 coalescing cites 395–399 | Drifted: `actor.rs:596` defines the existing 50ms window. Retraction gates before it. |
| Store cancel boundary in turnperf tables | Drifted: current `Store::cancel_turn` delegates to the shared transaction helper; Cancelling still commits before the wake. |
| Round-1 fsync cost hypothesis | Superseded by supplied FACTS2/PROPOSAL2: WAL/NORMAL transactions are not per-event device flushes. No performance claim or durability weakening here. |
| SIGKILL proof harness | Correct construct: `scripts/qa-gate/turnperf_sigkill_matrix.py`; script unchanged, executed against the fresh lane binaries. |

Minimal adjacent-territory edits are the worker configuration flag, startup
recovery recognition of the new response boundary, and the provider-stream
wrapper's bounded buffered-tail extraction. Provider transport retry logic,
worker retirement/delegation/waits, OAuth and OAuth tests are untouched.
Windows/Linux behavior is **by inspection**; executed host is macOS arm64.
No lane-authored test is ignored or platform-gated; upstream test cfg changes
are preserved exactly as merged.

## Named regressions

| Behavior | Test |
| --- | --- |
| Durable fact, attachments, restart receipt/CAS and terminal replay | `retract_preserves_append_only_prompt_attachments_receipt_and_terminal_replay` |
| Store writer race both ways; typed too_late; losing delta | `retract_first_response_serialization_proves_both_race_orders` |
| Fork hiding and nontrivial copied-cursor remap | `fork_rejects_hidden_prompt_boundary_and_remaps_retained_retraction_cursor` |
| Actor tie; no visible reply; provider closed; one request/settlement and released permit | `retraction_cancellation_tie_discards_the_already_ready_first_delta` |
| Retraction while response boundary is awaiting commit | `retraction_wins_while_the_first_response_boundary_is_waiting_to_commit` |
| Cold/warm/compacted/evicted provider history | `retracted_prompt_is_excluded_from_cold_warm_compacted_and_evicted_history` |
| Exact TUI node hiding and replay parity with equal-text neighbors | `retraction_hides_only_its_node_and_preserves_replay_and_later_anchors` |
| Hot/cold/crash sidecar reconstruction | `retraction_rebuilds_hot_cold_and_crash_reconciled_transcripts` |
| Controllable HTTP first byte, CAS attachment restoration, next provider body, export and exact live/replay envelopes | CLI `escretract_tests` |
| Empty/metadata vs content response boundary | protocol `retraction_tests` |
| RPC request/receipt/error golden | `turn_retract_request_receipt_and_too_late_are_golden` |

The first fork-test run exposed invalid test setup (forking a nonterminal run).
Both test runs now terminalize before the cut, and the hidden-cut assertion
checks the retraction-specific error. The actor test adapter's missing
`branch_lineage` forwarding was fixed after its initial compile check. Neither
failure required weakening a product assertion.

The first complete crate sweep also exposed older closed-enum assumptions:
two daemon assertions (feature count and branch events), fourteen daemond
live-RPC tests sharing typed-only event helpers, and the RPC contract example
inventory. The helpers now validate the exact named `response_started` fact,
including its semantic delta, durability and render policy; unrelated unknown
events still fail strict decoding. Branch affinity, exactly one boundary and
full-envelope live/replay equality are asserted. The feature pin is
117 → 118. Both new automation request/response examples come from the
tool-generated retraction wire golden, and the exact example count is 41 → 43.
These were test/contract integration corrections, with no production change
or removed lifecycle, usage, cursor, or transcript assertion.
The first daemond rerun reached a second strict decode in the vanished-workspace
test after its shared helper was repaired. That local loop now validates and
counts exactly one response boundary while retaining the workspace-notice and
missing-instruction checks. Its intermediate failure is retained separately.
The one-shot boot JSONL golden also exposed the new response boundary. It is
regenerated only through `HAIDER_ONESHOT_GOLDEN_UPDATE=1`, then checked in a
plain full CLI rerun; its existing semantic and volatile-field rules remain.
An exact structural comparison confirms one added response boundary plus
derived sequence increments, with every prior one-shot field preserved.

## Gate results

- Required ENV LAW used throughout; two Cargo build jobs and four test threads.
  Disk checked before every build-capable gate, with the 700 MiB stop floor.
  Fresh CLI/daemon siblings are built before daemon tests and
  `HAIDER_TEST_SIBLINGS_PREBUILT=1` is set. `haiderd` exceeds 10 MiB.
- `bash scripts/qa-gate/run.sh test`: **77 passed**, zero failures.
- Unchanged SIGKILL matrix: **52/52 passed**, zero failures; see
  [retained report](escretract-sigkill.json). Discovery still drives the sweep,
  so the new durable response boundary receives its own kill points.
- Merge-forward parity: all **70** upstream changed/new files match target
  `022ccde3` byte-for-byte. The matrix/proxy and protected OAuth files have no
  changes. Current binary hashes match the retained final SIGKILL report.
- Goldens were regenerated using the existing fixture update tests, then
  verified without update mode. `provider_request_no_budget.json` remains
  byte-identical. The measured instruct-pipe platform-invariant pin remains
  **5,592 → 5,592 bytes**, with **5,670 bytes** on this POSIX host; no prompt
  or tool schema changed. Source test markers are **5,033 → 5,049** (+16).
- `cargo test -p <crate> --no-fail-fast`, scoped sequentially across **all 17
  workspace crates**: **5,484 summed libtest passes, zero failures**, with
  **13 unchanged pre-existing ignores**. Daemon, daemond, RPC and CLI were
  rerun in full after their integration corrections. Source markers and
  summed libtest executions are different measurements. Per-crate counts,
  initial/intermediate failures and log hashes are retained in
  [gate summary](escretract-gates.json).
- Strict `cargo clippy` with `--all-targets -- -D warnings` passes for the
  eleven affected/merged crates (platform, protocol, provider, store, core,
  RPC, client, TUI, daemon, CLI and daemond). An additional daemond Clippy
  pass covers the last test correction after the full suite.
- `cargo fmt --all -- --check`, `git diff --check`, and both authoritative
  `xtask test-count --update` / `xtask test-count` pass; count **5,049/5,049**.
- The final full CLI test build replaced the CLI executable after the prior
  SIGKILL pass. The unchanged matrix was rerun and passed **52/52** against
  the current CLI and unchanged daemon; both hashes match the retained
  report. No production source changed during these gate repairs.

## Independent verifier value

Four unique confirmed observations changed the implementation/tests:
ready-delta cancellation ties, fork cursor remapping, missing export filtering,
and the separately missed unmasked JSON export constructor. Duplicate discovery
of the same fork issue counts once. All four are fixed. Two independent final
reviews return SHIP by inspection; executable gates remain a separate authority.
All executable gates above are green. Final lane verdict: **SHIP**.
