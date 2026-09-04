# v0.0.970 turnbudget

The request ceiling now defaults to a 32-request soft tranche and a separate
64-request hard cap. Reaching the tranche adds a durable typed instruction to
the model's next request. Reaching the hard cap returns
`request_budget_exceeded`, preserves the journal, and provides continuation
coordinates for a fresh turn on the same timeline. The full workspace suite,
affected-crate Clippy with warnings denied, and repository guards pass under
ENV LAW. The benchmark aggregate remains a cited claim rather than a rerun.

## CLAIM-AUDIT

Baseline is lane commit `05d00b48`, before this change. Brief citations were
audited against that baseline rather than treated as current line numbers.
The external investigation is
`/Users/rizzist/.claude/jobs/1a0b5b6c/tmp/971-benchmark-rootcause.log`.

| Claim | Audit | Evidence and consequence |
|---|---|---|
| The default request cap was 32 at `actor.rs:338`. | **CORRECT construct; DRIFTED citation.** | Baseline `crates/haider-core/src/actor.rs:339` defines 32; its config default is at `:790`. Current `actor.rs:345` defines 64 and `:800` retains tranche 32. |
| The cap was enforced at `actor.rs:3376`. | **CORRECT mechanism; DRIFTED citation.** | Baseline `actor.rs:3395` increments only when `provider_attempt == 0`; `:3398` rejects count above the limit before dispatch. Baseline `:11206` creates `LoopLimit`. The attempted 33rd logical request was never sent. |
| Seventeen of twenty measured Haider runs stopped at the cap, exit 70, with untouched workspaces. | **CITED benchmark claim; not independently reproduced.** | Investigation log `:25`–`:38` reports the aggregate and three paired transcript audits. The supplied log is the evidence, not the raw twenty-run artifact set. Baseline CLI `run.rs:34,1966` corroborates exit 70 for the generic loop-limit failure. No new success-rate claim is made. |
| The bound discarded partial work and tool history. | **WRONG as a general storage claim; CORRECT as a continuation usability defect.** | Baseline `actor.rs:9164`–`:9225` settles durable tools, `:9490` cleans up open items, and `:9520`–`:9545` commits adjacent `RunFailed`/`Errored`. Baseline/current `prompt_history.rs:4287` keeps prior terminal runs; baseline/current `:4390`–`:4418` reconstructs tools. `cancelled_and_errored_runs_keep_their_committed_history` already pins retention. An untouched workspace means no edits occurred, not that the cap rolled edits back. |
| Retrying the failed run resumes its completed assistant/tool history. | **WRONG for the existing retry path.** | Baseline/current `worker.rs:2701`–`:2720` reuses the original `prompt_run_id`; `:8317`–`:8324` compiles that run as current. Baseline `prompt_history.rs:4350` excludes current-run assistant/tools. A normal next user turn already retains them. The new continuation deliberately accepts a fresh run in the original session. |
| OpenCode has no default step cap (`agent.steps ?? Infinity`). | **CITED prior source audit, with version uncertainty.** | Investigation log `:24388` links tagged OpenCode v1.18.9 `packages/opencode/src/session/prompt.ts:1028`–`:1118` and `:1156`–`:1263`. The currently installed package identifies as 1.17.20, matching the provenance warning at log `:24274`; it does not authenticate the claimed benchmark version. No upstream fetch or benchmark-binary verification was performed for this lane. |
| A solved comparator run needed 53 requests. | **CITED benchmark claim; round count uncertain.** | Investigation log `:27,43` reports 53 total calls; `:24390` notes an optional title request, so this may represent 52 main rounds. The new deterministic 53-logical-request test conservatively verifies this workload shape fits. Missing raw benchmark journals and binary hashes are listed at log `:24469` onward; this lane does not reproduce the benchmark tasks. |
| A larger hard cap alone guarantees convergence. | **WRONG.** | The investigation also reports repeated reading/planning and malformed tool JSON. This lane removes the premature cap and supplies checkpoint feedback; it does not assert that those independent causes are fixed. |

**Number rationale:** 64 is two 32-request tranches and leaves eleven logical
requests beyond the reported 53-request solved workload (about 20.8% headroom).
It doubles the old ceiling while retaining a finite guard for a real loop.
The soft warning leaves another 32 requests for completion or an explicit
checkpoint. This is a policy choice informed by the supplied workload, not a
statistical guarantee. Both numbers can be configured.

All 22 supplied lens documents were read: `turnperf/{FACTS,PROPOSAL,MERGED}.md`
(including L1–L8) and `turnperf2/{FACTS2,TRACE-FINDINGS,PROPOSAL2,C1–C4,D1–D8,X1–X4}.md`.
Their relevant constraints are retained: the journal is the source of truth;
request-attempt persistence precedes provider dispatch; raw sequence numbers
remain the replay cursor; each run has one immutable terminal. Round 2 reports
an unsafe earlier admission-fusion experiment with a duplicate provider
request. This lane does not move the CAS barrier or combine session creation
with turn admission. It makes no latency, CPU, or memory improvement claim.
`TRACE-FINDINGS.md` is the later measured trace and supersedes
`PROPOSAL2.md`'s earlier statement that those trace artifacts were unavailable.

## Behavior and durable contract

`crates/haider-protocol/src/request_budget.rs` defines `RequestBudgetV1`,
`RequestBudgetPhaseV1`, `RequestBudgetStatusV1`, and
`RequestBudgetContinuationV1`. A policy requires `0 < tranche <= hard_cap`.
The additive extension kind is `provider_request_budget_v1`; its status carries
`used`, `budget { tranche, hard_cap }`, `phase`, and
`continuation { session_id, run_id, branch_id?, agent_id? }`.

The actor records progress for logical requests in the existing durable
request-attempt transaction. Retries of the same logical request do not spend
another request: the increment still depends on `provider_attempt == 0`.
Progress is prompt-omitted and rendered in the TUI as, for example,
`requests 31 / tranche 32 / hard cap 64`. Both styled and plain renderers decode
the typed data, including when the optional generic `label` is absent.

Before another logical request after the soft tranche, the actor commits one
`soft_bound` note, then appends its typed JSON and checkpoint instruction to the
actual provider message history. The soft state auto-continues. The note is
reconstructed from durable history on actor recovery, so recovery neither
resets spent requests nor emits the note again. The prompt compiler explicitly
handles this extension; generic extensions alone would otherwise be omitted
from model input. A response that finishes exactly at the tranche can complete
naturally; its final progress status still exposes the spent count.

At the hard cap the actor does not send the next logical request. Its
`hard_bound` extension is committed with the named typed failure and terminal
batch after preserving partial items. `ErrorCode::RequestBudgetExceeded`
serializes as `request_budget_exceeded`. CLI print/JSON output names the
continuation command and maps the cause to exit **77** (`EX_BLOCKED`), while
JSONL retains the ordinary lossless durable event stream and terminal. The old
run remains terminal; continuation never reopens or rewrites it.

The new run uses the same session and branch history with a fresh request
allowance. Its `HeadlessRunSpecV1.continuation_of` records the source run.
Store admission validates the source checkpoint and scope inside the ordinary
acceptance transaction: the source must be the latest terminal run on that
timeline, with no competing active turn. Reusing a consumed or stale handle
is rejected; receipt retries of an already accepted continuation retain the
same accepted identity. A database reopen preserves both the source checkpoint
and this admission fence.

`request_budget_v1` is a separate advertised feature. Clients require it for
an explicit request policy or `--resume`, so an older daemon cannot silently
ignore these additive fields. Old headless specs and spawn arguments omit the
new optional keys and preserve their historical serialized shape.

## Configuration and continuation

Per-run configuration:

```sh
haider run -p 'Complete the task' --request-tranche 32 --max-requests 64
haider run --resume RUN_ID
haider run --resume RUN_ID -p 'Continue with the retained checkpoint' \
  --request-tranche 40 --max-requests 96
```

`--request-tranche` or `--max-requests` alone uses the default for the other
request-policy field. Zero, reversed bounds, overflow, duplicate flags, and
incompatible lifecycle actions are rejected before execution. Resume can omit
the prompt, in which case a continuation instruction is supplied. The source
run's configured budgets are inherited unless overridden for the new run.
Session model and permission settings are inherited; resume rejects flags
that would silently change them. A new allowance is per turn; existing
token/cost/time policies continue to govern their own accepted run.

Per-agent model tool argument:

```json
{
  "task": "Implement and verify",
  "prompt": "Complete the scoped change and report the checks",
  "request_budget": { "tranche": 40, "hard_cap": 96 }
}
```

The `spawn_subagent` schema validates this argument, stores it in the child's
durable manifest coordinates, and applies it on subsequent child turns.
Worker precedence is explicit per-run policy, then the child's frozen policy,
then 32/64 defaults. Request caps are local to each actor; the existing shared
root/child token, cost, and time coordinator is separate. Request-only policy
does not install that coordinator or its usage polling monitor.

The headless `--resume RUN_ID` command currently supports root headless runs.
Branch checkpoints continue through a new turn in the owning branch/session;
child checkpoints continue through `message_subagent` in the owning agent.
The CLI rejects unsupported branch/agent scope instead of starting a different
timeline. The TUI displays the budget and continuation coordinates but adds no
new resume button. A soft-bound run must first finish or checkpoint to a
terminal state before a fresh continuation can be admitted; it continues
automatically until then. No claim is made that checkpoint summaries alone
replace exact tool history: the journal and ordinary prompt compiler retain
that history.

## Scope and cross-lane seams

Core actor enforcement, prompt reconstruction, protocol extensions, CLI/client
continuation, store admission, spawn configuration, and TUI rendering are the
changed behavior. `worker.rs` has only the configuration selection and shared
budget-coordinator filter changes; the existing money-budget checks, supervisor
retirement, and workflow rebind logic are preserved. Recovery reads the
durable logical count for recovered checkpoints. The one-line
`turn_recovery.rs` admission predicate now uses `has_shared_limits()` to retain
its original token/cost/time meaning: a request-only policy must not widen
ambiguous first-dispatch replay. The feature token is added
to the daemon connection advertisement and its exact-set test. Optional-field
fixture constructors are updated across consumers. No OAuth implementation or
OAuth test file is changed. Supplied lane briefs and lens evidence remain
uncommitted; this report is the new testing-document deliverable.

The unsafe-count guard exposed inherited baseline drift: four existing TUI
test forwarding unsafe blocks in one `GlobalAlloc` implementation at
`tuivirt_memory_tests.rs:54,62,71,75` were absent from `ci/unsafe-counts.json`.
The verifier traced their introduction
to `c0a79b37e` and later changes to `8430c886`. Only the TUI test baseline changes
from 0 to 4; this lane adds no unsafe code. The corrected guard reports 189
production and 20 test unsafe sites.

## Named verification

Every named test below passed in the final workspace sweep. Commands, counts,
and evidence logs are recorded after the table.

| Contract | Named tests | Result |
|---|---|---|
| Soft note once, typed and in actual model request | `soft_request_bound_is_once_typed_and_in_the_actual_model_request` | PASS |
| Hard cap, partial text/tool history, fresh-turn restoration | `hard_request_bound_restores_partial_text_and_tool_history_after_actor_restart` | PASS |
| Hard checkpoint and terminal atomicity, including failed append | `hard_request_checkpoint_and_named_terminal_share_one_atomic_append`; `rejected_hard_request_checkpoint_exposes_neither_handle_nor_terminal` | PASS |
| Retries excluded at both bounds | `request_budget_ignores_transport_retries_at_the_soft_and_hard_bounds` | PASS |
| Reported 53-round shape completes | `default_request_budget_covers_the_reported_fifty_three_round_workload`; `default_budget_completes_fifty_three_logical_requests` | PASS |
| Recovered child checkpoint retains count and note | `recovered_child_checkpoint_restores_budget_even_without_legacy_count` | PASS |
| Store reopen and one-time continuation admission | `budget_continuation_survives_reopen_and_is_consumed_once_at_admission` | PASS |
| Client same-session continuation | `resume_budget_checkpoint_submits_new_turn_in_original_session`; `resume_rejects_active_and_nonbudget_sources`; `resume_accepts_terminal_soft_and_hard_checkpoints_and_checks_scope`; `resume_inherits_budgets_and_applies_only_explicit_new_caps` | PASS |
| Typed protocol/old serialization | `request_budget_defaults_allow_two_tranches_and_validate_order`; `legacy_run_budget_omits_request_policy_and_new_pin_roundtrips`; `request_budget_extension_retains_typed_coordinates_and_phase_specific_model_note` | PASS |
| Per-agent schema, persistence, and resumed tool history | `child_request_budget_is_validated_and_preserved_in_spawn_arguments`; `established_spawn_captures_parent_branch_and_replays_one_child`; `message_subagent_resumes_hard_bound_child_with_retained_tool_history` | PASS |
| TUI without generic label | `budget_status_without_label_renders_counts_and_continuation_in_both_surfaces` | PASS |
| CLI configuration and exit | `request_budget_flags_preserve_defaults_and_reject_invalid_or_duplicate_limits`; `resume_parser_accepts_handle_and_budget_without_prompt`; `request_budget_exit_is_blocked_and_json_preserves_resume_instruction` | PASS |
| Existing low-cap workflow/input pins | `headless_workflow_provider_request_cap_returns_resumable_budget_cause`; `provider_request_ceiling_preserves_a_typed_budget_continuation` | PASS |
| Request-only policy preserves first-dispatch recovery rule | `only_budgeted_active_workflow_admission_is_runnable_recovery_work` | PASS |

The previous `LoopLimit` pins retain their exact no-extra-request assertions;
their cause expectations change because request exhaustion is now a resumable
budget cause. No test was newly ignored, deleted, or weakened to make the gate green.

Required environment for every build/test:

```sh
RUST_MIN_STACK=8388608 HAIDER_DISCOVERY_DISABLED=1 \
HAIDER_TEST_DEVICE_NAME=test-mac CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0
```

Check `df -m /` before each build and stop below 700 MiB. Prebuild the current
`haiderd` and `haider`, verify `haiderd` exceeds 10 MiB, then set
`HAIDER_TEST_SIBLINGS_PREBUILT=1`. The full workspace test is mandatory; a
successful check or selected crate suite is not equivalent. Clippy is scoped
by affected crate to respect the shared-host instruction against
`cargo clippy --workspace --all-targets`.

| Final gate | Result / artifact |
|---|---|
| `cargo test --workspace --locked --no-fail-fast` under ENV LAW | PASS, exit 0; 5,177 passed, 0 failed, 13 existing ignores across 325 result records, including doctests and subprocess reruns; these are aggregate results, not unique test definitions. `/tmp/turnbudget-workspace-final.log`. |
| Affected-crate clippy, all targets, `-- -D warnings` | PASS; `/tmp/turnbudget-clippy.log` |
| Repository guards, format, `git diff --check` | PASS: unsafe-count guard 189 production / 20 test; `xtask check` (nine existing soft LOC warnings); rustfmt for all 41 changed Rust files; diff check. Logs `/tmp/turnbudget-unsafe.log`, `/tmp/turnbudget-guards.log`. |
| `cargo run -p xtask --locked -- test-count --update`, then check | PASS: 4,764 → 4,786 (+22); `/tmp/turnbudget-test-count.log`; checked again by `xtask check`. |
| Sibling binaries current and intact | PASS: final rebuild `/tmp/turnbudget-build-final.log`; `target/debug/haiderd` 198,058,976 bytes (>10 MiB), `target/debug/haider` 109,139,568 bytes; both Mach-O arm64. |
| Independent verifier | **SHIP** — Curie independently reviewed the implementation, golden diffs, complete workspace log/totals, Clippy, and guard evidence. No remaining correctness blocker. |
| Lane commit | **BLOCKED by managed filesystem policy.** `git add --pathspec-from-file=/tmp/turnbudget-stage-paths --pathspec-file-nul` exited 128: Git could not create `/Users/rizzist/haider-run/haider-agent/.git/worktrees/lane-970-turnbudget/index.lock` (`Operation not permitted`). The worktree is writable but its Git metadata is outside the writable roots. No commit or push occurred. |
| Benchmark success-rate rerun | NOT RUN; no improvement claimed |
| Linux and Windows execution | NOT RUN locally; platform behavior by inspection only |

The first complete workspace sweep exposed twelve stale expectations or fixture
issues across five targets. Exact journal pins now include two budget events per
logical request and their consequent sequence/item-ID shifts. The wire ledger
adds only the optional subagent request-policy schema. Feature count changes
from 111 to 112, and full/stub tool-schema byte pins grow by 367/143 bytes;
tool counts and the existing minimum reduction assertion remain intact. Valid
prompt checkpoints use the new reducer version. The recovered-child fixture
now ends at a realizable committed tool boundary and supplies the dispatcher
required by child settlement. These are fixture corrections, not waived failures;
the final full sweep is authoritative. Initial log:
`/tmp/turnbudget-workspace.log`; golden regeneration:
`/tmp/turnbudget-goldens.log` (all ten golden tests passed).

## CI error-registry walk

Read in full:
`/Users/rizzist/.claude/projects/-Users-rizzist-Documents-CODING/memory/haider-ci-error-registry.md`.
It contains classes 1–86 (86 repeated in the source) and 94/95; the repeat is
audited once. Applicability was reviewed by inspection and checked against
the completed macOS workspace, Clippy, and guard results above. Linux and
Windows remain by inspection only.

| Class | Audit |
|---:|---|
| 1 | fixed: additive `RunBudgetV1`/`HeadlessRunSpecV1`/spawn fields audited across source and fixture constructors; workspace compile/test passed. |
| 2 | fixed: typed request-policy getters and changed actor/config call sites aligned; compilation passed. |
| 3 | checked: continuation specs clone source pins before new-run mutation; ownership/compilation gate passed. |
| 4 | checked: tests use public protocol constructors and existing projection APIs. |
| 5 | checked: no new platform-only imports; Linux/Windows by inspection. |
| 6 | fixed: `FEATURE_REQUEST_BUDGET_V1` added once and expected standard feature set updated. |
| 7 | checked: none — no dependency or lockfile edits. |
| 8 | fixed: optional fixture insertions re-read after mechanical update; final compilation passed. |
| 9 | checked: new conditions use existing let-chain style; Clippy passed with warnings denied. |
| 10 | checked: new helpers are called by actor/client/TUI; Clippy passed with warnings denied. |
| 11 | checked: borrowed/optional fields reviewed; Clippy passed with warnings denied. |
| 12 | checked: configuration and continuation travel in typed structs. |
| 13 | checked: none — no new complex public callback signatures. |
| 14 | checked: policy/status fields support `Eq`; opaque JSON remains outside these structs. |
| 15 | checked: none — no iterator-direction sweep. |
| 16 | checked: nonzero/order validation has no platform literal-range assumption. |
| 17 | checked: no new mutex guard spans an await; store validation remains inside its transaction. |
| 18 | checked: new tests use file/module test allowances; production serialization errors are returned. |
| 19 | fixed: touched-file formatting and diff check passed. |
| 20 | fixed: test-count regenerated for the final code and checked at 4,786 (+22); no deleted or ignored tests. |
| 21 | checked: ENV LAW retains 8 MiB stack. |
| 22 | checked: none — no process-global subscriber installation. |
| 23 | checked: none — no migration; continuation uses existing journal/receipt tables. |
| 24 | checked: none — provider catalog authority unchanged. |
| 25 | checked: none — no render timing or performance numbers claimed. |
| 26 | checked: no new platform filesystem API; store reopen uses existing abstraction. |
| 27 | checked: resume reuses the existing negotiated connection/reconnect machinery; Windows by inspection. |
| 28 | checked: no new process-tree harness or Windows serialization policy. |
| 29 | checked: connect/spawn authorization unchanged. |
| 30 | fixed: directed request-count/terminal tests assert named causes and retained history; runtime gate passed. |
| 31 | checked: none — no Android edits. |
| 32 | checked: none — no release/publish action. |
| 33 | checked: none — no runner-wide env/serialization change. |
| 34 | checked: none — only existing serde/protocol dependencies used. |
| 35 | checked: none — no ambiguous dependency trait calls introduced. |
| 36 | checked: owned continuation pins outlive awaits; compile gate passed. |
| 37 | checked: no new cfg-specific type seam; Linux/Windows by inspection. |
| 38 | checked: continuation comparisons use existing typed IDs, not wire objects as keys. |
| 39 | fixed: changed cross-crate test constructors audited; full workspace gate passed. |
| 40 | checked: none — no new platform dependency-error conversion. |
| 41 | checked: new integration tests retain existing short isolated roots. |
| 42 | checked: both current siblings rebuilt before subprocess tests; all subprocess tests passed. |
| 43 | checked: none — no pre-exec descriptor sweep edits. |
| 44 | checked: socket test evidence must be actual gate output; no inspection-only pass. |
| 45 | checked: none — no new unsafe code. |
| 46 | checked: none — runtime root ownership policy unchanged. |
| 47 | checked: none — no filesystem walker change. |
| 48 | checked: new integration test files are in `tests/`; core support module is explicitly declared. |
| 49 | checked: none — queued native-pipe acknowledgement logic unchanged. |
| 50 | checked: new budget values/notes are platform-neutral; tool schema size pins updated for the additive field; full workspace passed. |
| 51 | checked: none — profile lock contents unchanged. |
| 52 | checked: CLI syntax updated; TUI help viewport unchanged. |
| 53 | checked: none — runtime mode/owner handling unchanged. |
| 54 | checked: corrected registry ENV LAW retained; no later unrun binary treated as passing. |
| 55 | checked: none — no new cfg-dependent unit-valued binding. |
| 56 | fixed: `RequestBudgetExceeded` maps by typed cause to CLI exit 77 in every terminal phase. |
| 57 | fixed: styled/plain budget projections share typed summary and one renderer test; passed. |
| 58 | checked: none — tool result inline/CAS thresholds unchanged. |
| 59 | checked: none — account roster grammar unchanged. |
| 60 | checked: none — connection/process liveness mechanism unchanged. |
| 61 | fixed: report distinguishes source inspection, cited benchmark aggregate, and completed executable evidence. |
| 62 | checked: new public helper return types have audited callers; no existing `()` API retargeted. |
| 63 | checked: none — no new shell/platform archive command. |
| 64 | checked: disk checked before each build; final sweep started with 16,468 MiB available. Both current sibling binaries are Mach-O arm64; daemon is 198,058,976 bytes. |
| 65 | checked: none — no raw-errno terminal mapping added. |
| 66 | checked: none — STT unchanged. |
| 67 | checked: prebuilt both siblings for CLI and client suites; passed. |
| 68 | checked: none — no cleanup-error policy change. |
| 69 | checked: none — executable path casing unchanged. |
| 70 | checked: none — no workflow trigger or CI dispatch. |
| 71 | checked: real daemon/client integration evidence required; no shipped-binary success claimed. |
| 72 | checked: discovery intentionally disabled by ENV LAW; no credential-discovery claim. |
| 73 | checked: new tests exercise behavior and typed records, not fixed source-byte windows. |
| 74 | checked: subprocess tests retain temporary machine-user home/profile isolation. |
| 75 | checked: none — no new hub-owned task/channel lifecycle. |
| 76 | fixed: budget counts/continuation projected in TUI and CLI; JSONL retains typed extension and terminal. |
| 77 | checked: repository guards passed before final gate acceptance. |
| 78 | checked: none — no tag/release dispatch. |
| 79 | checked: none — render performance bounds unchanged. |
| 80 | checked: none — no new blocking CI job. |
| 81 | checked: current siblings rebuilt before `SIBLINGS_PREBUILT=1`; passed. |
| 82 | checked: OAuth files untouched; any observed timing failure must be reported and isolated, not edited away. |
| 83 | checked: hook timing failures must be named and isolated if observed; no hook test weakening. |
| 84 | checked: native-pipe coverage-tail failures must be named and rerun if observed. |
| 85 | fixed: full workspace tests passed for all additive feature/schema cross-consumers. |
| 86 | fixed: independent SHIP requires `cargo test --workspace`; check-only evidence cannot satisfy this lane. |
| 94 | checked: no new product wait budget; continuation reuses continuous existing deadlines, directed test waits must document their enclosing allowance. |
| 95 | checked: resume uses the existing transport reader; integration observers must service Ping/Pong during retained-connection waits. |

## Final disposition

**Implementation: SHIP. Delivery: NO_SHIP.** Required executable gates are
complete, and the independent verifier returned SHIP. The requested lane commit
could not be created because the managed filesystem policy rejects writes to
the sibling repository's Git metadata. All changes remain in
`lane-970-turnbudget`; a complete patch is saved at `/tmp/turnbudget.patch`, with
the 51 intended paths in `/tmp/turnbudget-stage-paths`. Supplied lane briefs and
lens evidence are excluded. No commit or push occurred. Benchmark success-rate
and non-macOS runtime claims remain outside the verified result.

## Merge-forward continuation (2026-09-05)

This continuation supersedes the delivery disposition above: the orchestrator
has committed the implementation, and this task requires an **uncommitted**
resolved merge tree. Actual starting HEAD was `702ae92c`, whose parent is
`53dc49f7`; its only change removes the 24 supplied brief/lens documents from
tracking. Those documents remain present and untracked. The incoming ref is
`origin/wave-970` at `e1aca96c6f0a292859b5e69338f9649c563454df`.
Turnid was already in the starting ancestry through `05d00b48`.

The requested fetch failed while opening `FETCH_HEAD`, and the direct merge
failed while locking `ORIG_HEAD`, because the worktree Git directory is outside
the writable sandbox. The current local remote ref was therefore used. A
temporary Git directory with read-only access to the existing object database
performed the real three-way `git merge --no-commit origin/wave-970` against
this worktree. It reproduced exactly the six reported conflicts. Its location
and parent IDs are recorded in `/tmp/turnbudget-merge-state.json`; the directory
is `/var/folders/y2/zrvhkfz54lj3gsw2czwxdmsh0000gn/T/turnbudget-merge-o3ytxyvo/git`.
All six paths are resolved in that temporary index. Original HEAD and original
Git metadata remain unchanged; the orchestrator must record the merge parents
and stage the resolved files. No commit or push was attempted after resolution.

### Resolution and complete fixture review

No fixture was hand-merged or copied from either parent. The conflicted files
were overwritten by their existing test regeneration paths, using the freshly
built merged daemon/client. The oneshot test used
`HAIDER_ONESHOT_GOLDEN_UPDATE=1`; the turnhygiene suite used
`UPDATE_FIXTURES=1`. Nine turnhygiene tests and the selected oneshot test passed.

| Conflicted file | Resolution and review |
|---|---|
| `crates/haider-daemon/src/permissions_core_tests.rs` | Retained actbias's scoped native-description checks and exact description-content test, plus turnbudget's schema byte growth. Full-prefix pins are Linux 18,621, macOS 18,572, Windows 18,571, other 18,566. Instruct pipe is `12,122 + 143 + 499 = 12,764`; subtracting the five descriptions still pins 12,265. Tool counts and the minimum 30% reduction assertion remain unchanged. Non-macOS values are by inspection. |
| `crates/haider-cli/tests/fixtures/oneshot_run_golden.jsonl` | Regenerated all 24 lines. Against starting HEAD, only line 21 changes `haider-system-v3` to `haider-system-v4`. Both correlation payloads and both budget progress events remain byte-identical. |
| `crates/haider-cli/tests/fixtures/turnhygiene/provider_request_no_budget.json` | Regenerated its single complete request line. Against starting HEAD, exactly six JSON paths change: shared policy +366 bytes and the five native descriptions totaling +499 bytes. Against wave, the only addition is the exact retained `spawn_subagent.request_budget` schema. No correlation field is added to the provider HTTP body; correlation belongs to the durable attempt records in the JSONL fixtures. |
| `crates/haider-cli/tests/fixtures/turnhygiene/run_jsonl_text_turn.jsonl` | Regenerated all 24 lines. Only line 21 changes the system-version identity. Both correlation payloads and both budget events remain byte-identical. |
| `crates/haider-cli/tests/fixtures/turnhygiene/run_jsonl_tool_turn.jsonl` | Regenerated all 53 lines. Only line 50 changes the system-version identity. All four correlation payloads (request ordinals 1, 1, 2, 2) and four budget events remain byte-identical. |
| `test-baseline.txt` | Replaced the conflict through `cargo run -p xtask --locked -- test-count --update`, then checked without `--update`: 4,788/4,788. This is +2 over starting HEAD's 4,786 and +22 over wave's 4,766. |

The JSONL line review includes every unchanged line, its sequence, acceptance,
tool output, effect, and terminal. The audit is saved at
`/tmp/turnbudget-merge-fixture-audit.log`; independent verification repeated the
comparison directly from both Git parents. No normalization was added to the
repository's tests. Existing identity/hash/estimate normalization remains the
only volatile-field handling. The real HTTP request ledger independently pins
the complete prompt bytes.

Clean auto-merges retain the incoming prompt/version and selective-description
changes in `worker.rs`, exact policy checks in `project_instructions_tests.rs`,
and five filesystem manifest descriptions in `filesystem.rs`. Incoming
`actbias.md` and `CI_REGISTRY_WALK_QAGATE3.md` are preserved. Recovery remains
`startup-turn-recovery-v8`, including its exact pin; correlation validation and
the logical-budget/physical-retry distinction remain intact. No OAuth file
was changed.

All 22 supplied lens documents were read again. Their historical line numbers
were checked for the mechanisms relevant to this merge: SQLite NORMAL is now
`event_store.rs:193`–`:200`; the attempt commit remains before dispatch at
`actor.rs:3932` and `:3983`; budget progress uses that same append transaction;
recovery v8 is at `turn_recovery.rs:116` with its pin at `:2341`. The JSONL
cursor citation at contract line 15 remains correct; the terminal discussion
has drifted to lines 98–143. The round-2 unconditional-estimator claim is stale
for current code, which already gates estimation. No latency, CPU, memory,
benchmark success-rate, or cross-platform runtime improvement is claimed.

### Continuation CI error-registry walk

The full registry and the per-class walk above were reread for this merge.
The unchanged classes retain their scope assessment; this table records the
merge-specific updates, including the newly appended class 87.

| Class | Continuation audit |
|---|---|
| 1–6, 8, 39 | checked: both source intents remain; no field/constructor/import was removed to resolve the merge. |
| 7 | checked: none — manifests and lockfile are unchanged. |
| 9–18, 87 | checked: the required gate includes test code, using exactly `cargo clippy --workspace --tests -- -D warnings`. Incoming policy constant assertions retain the `const` form. |
| 19 | checked: changed Rust files pass rustfmt; resolved diff has no whitespace errors. |
| 20 | fixed: regenerated `test-baseline.txt` to 4,788 through xtask, then checked it. |
| 21, 33, 54 | checked: ENV LAW retained, including 8 MiB stacks; no tests ignored, skipped, or platform-gated for this continuation. |
| 42, 64, 67, 81 | checked: fresh siblings were built before setting the prebuilt flag. `haiderd` is Mach-O arm64, 198,060,144 bytes; `haider` is 109,139,568 bytes. Disk was checked before each Cargo build/test command against the 700 MiB stop floor. |
| 50 | fixed: `permissions_core_tests.rs` retains the platform-specific full-prefix offsets, both additive byte deltas, and the common 30% law. |
| 61, 71, 72 | checked: no release-binary, discovery-enabled, performance, or non-macOS execution claim. |
| 77 | checked: unsafe-count gate passes, production 189 / test 20. |
| 82–84 | checked: no changes to OAuth, hook timing, or native-pipe coverage tests. Any gate failure is recorded by its actual test name. |
| 85–86 | checked: the required verification is the full workspace test, with fixture update flags absent; a compile/check result cannot substitute for it. |
| 94–95 | checked: none — this merge adds no deadlines, external waits, or transport behavior. |

### Completed continuation gates

Every Cargo command ran with
`RUST_MIN_STACK=8388608 HAIDER_DISCOVERY_DISABLED=1 HAIDER_TEST_DEVICE_NAME=test-mac CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0`.
After the fresh sibling build, tests, Clippy, and guards additionally used
`HAIDER_TEST_SIBLINGS_PREBUILT=1`. The full-gate runner explicitly asserted that
both fixture-update environment variables were absent. Commands, exits, and
durations are recorded in `/tmp/turnbudget-merge-gate-results.json`.

| Gate | Final result and evidence |
|---|---|
| `cargo test -q --workspace --no-fail-fast` | **PASS, exit 0**: 5,183 passed, 0 failed, 13 existing ignores, 0 measured across 329 result records. These are emitted aggregate totals, including doctests and subprocess reruns, rather than unique source definitions. The nested harness output also records 1,373 filtered tests; the outer command applies no filter or skip. Log: `/tmp/turnbudget-merge-workspace.log`; counts: `/tmp/turnbudget-merge-test-totals.json`. |
| `cargo clippy --workspace --tests -- -D warnings` | **PASS, exit 0**, with test code included and no warning allowance added. Log: `/tmp/turnbudget-merge-clippy.log`. |
| `cargo run -p xtask --locked -- check` | **PASS, exit 0**: baseline 4,788/4,788; nine pre-existing soft LOC warnings. Log: `/tmp/turnbudget-merge-guards.log`. |
| Fixture regeneration | **PASS**: nine turnhygiene tests and the selected oneshot golden test. All four regenerated files were subsequently verified again by the full gate with no update flags. Logs: `/tmp/turnbudget-merge-turnhygiene-regenerate.log`, `/tmp/turnbudget-merge-oneshot-regenerate.log`. |
| Format, unsafe guard, resolved diff | **PASS**: rustfmt on all four merged Rust files; unsafe counts 189 production / 20 test; `git diff --check`; no remaining conflict markers or unmerged entries in the temporary merge index. |
| Gate/tree parity | **PASS**: all 11 merge-input hashes in `/tmp/turnbudget-merge-verified-inputs.json` remained unchanged through both gates. Only this testing report was appended afterward. |

The continuation patch and its exact path list are saved at
`/tmp/turnbudget-merge.patch` and `/tmp/turnbudget-merge-paths.txt`. The supplied
`LANE-COMMON.md`, `LANE-BRIEF-turnbudget.md`, `turnperf/`, and `turnperf2/` remain
untracked and excluded. Original HEAD stays `702ae92c`; merge parent remains
`e1aca96c`. The resolved working tree is left uncommitted for the orchestrator.

Final independent verification returned **SHIP** after reviewing both parent
diffs, every regenerated fixture, completed gate logs/totals, baseline, and
unchanged gate-input hashes.

`VERIFIER: findings=0 real=0 noise=0 — no findings.`
