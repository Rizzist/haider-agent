# v0.0.970 ceilingdecl

## Claim audit

Audited base: `73fe3f68f71b6daffffbb330beb9fabd27141cb7`, initially both
HEAD and cached `origin/wave-970`. Read the supplied lane common/brief and
turnperf/turnperf2 evidence before implementation; those inputs remain untracked
and are excluded from the deliverable.

| Claim left by turnbudget | Audit result |
|---|---|
| Hard request cap | Already 64 logical requests with a soft tranche of 32; transport retries use separate physical ordinals. |
| Terminal cause | Already typed `RequestBudgetExceeded` / `request_budget_exceeded`, ending `RunFailed` + `Errored`. Hard checkpoint and terminal already shared one append. |
| Exit 70 at the request cap | **Wrong for this base**: the cap mapped to 77 (`EX_BLOCKED`), shared with permissions, shared budgets, and unfinished workflows. This lane assigns dedicated 78. |
| Run result names `end_reason` | Missing from `HeadlessRunResult` and `RunJson`; added in their typed `terminal` block. |
| Partial progress in final result | Original text/tools/checkpoint existed in the journal, but the aggregate result lacked tree receipts, file counts/paths, tool-call count and last ordinal. Added retained terminal evidence. |
| Adapter declares ceiling | No `bench/` or product adapter manifest exists. Exact owner-paste TOML below; no product manifest parse test applies. |

Historical citations were checked by construct rather than trusted line number.
The D7-5/X1-7 workspace receipt references to worker entry 304, nonrepository
fallback 345, initial blocking worker 901 and post-receipt path 1005 remain
correct in `crates/haider-tools/src/workspace_receipt.rs`; its repository walk
reference 614 drifted to 615. Its bounded/unknown receipt cannot establish
untouched for an arbitrary non-Git benchmark tree, so it was not reused.
Round-1's WAL/NORMAL correction and round-2's keepalive/durability requirements
remain applicable; the new baseline piggybacks the existing first-attempt
append instead of adding a transaction, and capture uses `spawn_blocking`.
No latency, CPU, RSS or cross-platform runtime improvement is claimed.

## Retained contract

The headless actor captures its configured workspace before first dispatch.
Sorted content/type/permission receipts include hidden and ignored files,
directories and symlink targets without traversing links. Administrative `.git`
files/directories are excluded. The complete baseline is serialized as ordered
16 KiB UTF-8 chunks, with part count and complete-content digest, in hidden
`turn_workspace_before_v1` extensions. All chunks commit with the first provider
attempt. Same-run recovery uses that original baseline, not the current tree.

At the hard bound, post-receipt comparison creates the typed terminal block:

```json
{
  "end_reason": "harness_internal_ceiling",
  "internal_cap_detected": true,
  "exit_code": 78,
  "ceilings": { "soft": 32, "hard": 64, "used": 64 },
  "continuation": { "session_id": "SESSION", "run_id": "RUN" },
  "workspace_state": "untouched",
  "workspace_before": "blake3:IDENTITY",
  "workspace_after": "blake3:IDENTITY",
  "partial_progress": {
    "files_written": [],
    "files_deleted": [],
    "tool_calls": 64,
    "last_request_ordinal": 64
  }
}
```

This same block is retained on the terminal `run_state` payload and exposed
as `terminal` by the run-result API, `--output json`, and durable `--replay`.
Print output also names partial progress and the continuation. Its append is
atomic with the existing hard checkpoint, failure, and terminal. Existing
`terminal_kind` compatibility remains unchanged. Baseline records never enter
provider history. Replaying a capped run emits the original observations even
after files change; replay itself exits 0 on successful journal verification
and makes no provider requests. The recorded terminal still declares 78.

File lists are net tree changes, not attribution or a count of transient writes
that were reverted. Each durable `ToolResult` counts once, including sequential
provider requests reusing a call ID. `used` counts logical requests;
`last_request_ordinal` includes physical retries/auxiliary requests.

An unavailable observation cannot truthfully satisfy an always-present binary
workspace classification. Explicit exception: a detected read/race/special-file
failure retains cap 78, request counts, tool-call count and continuation, with
typed `workspace_receipt_error {phase: before|after, detail}`. Workspace state,
post identity and unavailable file lists are omitted; neither a third state nor
a false untouched value is emitted. Failure before first dispatch does not
block an otherwise valid chat run. Legacy recovery with no retained baseline
uses the same explicit unavailability. Old capped journals are not backfilled.

Complete capture requires O(total included file bytes) I/O and O(paths) receipt
storage. This includes ignored build trees. Chunks bound individual receipt
records, not total storage. Capture is sequential, not a filesystem-atomic
snapshot; concurrent external writes and active background tasks limit exact
point-in-time attribution. No new deadline or keepalive wait was added.

## Adapter declaration for harness-bench owner

Paste this exact optional block into the haider-agent adapter manifest. The
adapter's workspace substitution must bind `{workspace}` to the same canonical
directory passed as the run's cwd. The command must retain default 32/64 policy;
if the adapter overrides `--max-requests`, declare that resolved hard value.

```toml
[fidelity]
# Logical provider requests; soft tranche 32, hard ceiling 64.
declared_turn_ceiling = 64
# Integer process statuses, exclusively the logical request ceiling.
internal_cap_exit_codes = [78]
workspace_path = "{workspace}"
```

78 is the stable declared cap exit; 70 remains software failure, 77 remains
permissions/shared token-cost-time budgets/workflow blocking, 65 provider
failure, 76 protocol mismatch, 124 timeout and 130 cancellation. Detection uses
this typed declaration/exit or the validated typed terminal, never a regex on
model text or a soft-bound warning. No manifest is falsely claimed installed.

## Verification and merge status

Required merge-forward was attempted before gating. Both `git fetch origin
wave-970` and `git merge --no-commit origin/wave-970` were refused by filesystem
permissions on shared Git metadata (`FETCH_HEAD` / `ORIG_HEAD.lock`). HEAD and
cached origin were equal at that attempt. During the gate, cached origin
advanced to `38359fd3ba799c3e32a09c414f6f41abb90442bd` (providerrebind); HEAD
remains `73fe3f68f71b6daffffbb330beb9fabd27141cb7`. The refreshed fetch and merge
attempts failed with the same permissions errors, recorded in
`/tmp/ceilingdecl-merge-final.json`. Incoming changes overlap `actor.rs`, core
`lib.rs`, `worker.rs`, the client contract, event changelog and test baseline.
The full gate below validates this
**unmerged checkout**, not that newer upstream. Merge-forward and a new gate
on the merged result remain required. No hand-applied merge substitute was
used. The environment grants read-only Git metadata and no approval escalation.

The explicit user instruction to commit supersedes the common lane note to
leave work uncommitted. Staging exactly the 23 deliverable files was attempted
and failed with exit 128: shared `index.lock` creation is not permitted.
Consequently no commit could be created; no push was attempted. The supplied
lane briefs and turnperf evidence were excluded from the staging command.
The exact attempt is recorded in `/tmp/ceilingdecl-commit-attempt.json`.

Independent preliminary verifier: four findings, all accepted: receipt errors
erased the cap cause; reused call IDs undercounted calls; one unbounded receipt
envelope could exceed IPC frame limits; and full-tree scanning delayed cancellation. Fixed with explicit receipt diagnostics,
durable-result row counting, bounded receipt chunks, and cooperative cancellation between file chunks, with regression tests.

## CI error-registry walk

Read the supplied `haider-ci-error-registry.md` memory registry.

| Classes | Assessment |
|---|---|
| 1–8 | Checked: additive protocol and headless result constructors aligned; existing dependency set and lockfile retained. |
| 9–19, 34–40, 87 | Checked by compiler, rustfmt and strict Clippy including tests; no production lint allowance or unsafe code added. |
| 20 | Test baseline recounted through xtask, not hand edited. |
| 21–22, 28, 30, 33, 42, 54, 64, 67, 74, 81, 92 | ENV LAW, disk floor and fresh sibling identity retained; hermetic subprocess homes/workspaces; existing bounded process helper reused. |
| 23–27, 29, 31–32, 41, 43–46, 48–49, 51–53, 55–63, 65–66, 68–73, 75–76, 78–80, 93 | Checked: no migration/catalog/provider/transport/release/platform-process behavior changed. Non-macOS receipt code is by inspection only. |
| 47 | Hidden ancestors and non-Git trees covered by receipt tests. |
| 50, 88 | Provider prompt/tool schemas unchanged; fixture regeneration and instruct-pipe pin checked on final tree. |
| 77, 85–86 | Repository guards plus full workspace test required; compilation is not substituted for tests. |
| 82–84, 90 | Existing load-sensitive tests retained; any failures recorded by name. |
| 89, 91 | No save/restore merge recreation; no Git writes available. Cached parent was initially identical, then advanced during gating; merged-tree validation remains blocked. |
| 94–95 | No new deadlines; receipts off executor; original client/daemon transport loops continue servicing keepalive. |

## Completed focused verification

All Cargo commands used the required ENV LAW. Fresh siblings preceded
`HAIDER_TEST_SIBLINGS_PREBUILT=1`; both final binaries are Mach-O arm64. `haiderd` is
199,959,040 bytes (>10 MiB), `haider` 110,651,264 bytes. Disk was checked before
commands and remained above the 700 MiB floor.

| Check | Result |
|---|---|
| Protocol ceiling evidence | 1 passed; malformed/free-text/soft evidence rejected. |
| Complete receipts and cancellation | 11 passed; content, same-size edits, deletion, hidden/ignored entries, symlinks, modes, unavailability and cancellation. |
| Actor request-budget laws | 11 passed, including 5 new ceiling tests: exact untouched progress, multi-chunk recovery, reused IDs, pre/post observation failures. |
| CLI turnhygiene | 10 passed, including both workspace states and exact replay after later mutation. |
| One-shot golden regeneration | 1 passed. |
| Owner-paste TOML | Python `tomllib` parse pins integer 64, integer array `[78]`, and workspace template string. No installed/product manifest is claimed. |
| Formatting and diff | All 15 touched/new Rust files rustfmt-clean; `git diff --check` clean. |
| Unsafe guard | Pass: production 189, tests 20, unchanged. |
| Test count / repository guards | `xtask test-count --update`: **4,886 → 4,904**; `xtask check`: 4,904/4,904, with existing soft LOC warnings. |

Three JSONL goldens were regenerated through the tests, never hand merged:
`oneshot_run_golden.jsonl`, `turnhygiene/run_jsonl_text_turn.jsonl`, and
`turnhygiene/run_jsonl_tool_turn.jsonl`. Each gains the hidden receipt
Started/Completed pair; later sequences and item/event ordinals advance.
The full HTTP `provider_request_no_budget.json` fixture was regenerated and
remains byte-identical. No new normalization rule was introduced. The
instruct-pipe pin remains **13,552 → 13,552** on the tested source; it must be
rechecked after the required merge. No speculative re-pin was made.

Development failures were fixed before the final gate: two internal test
constructors needed the optional receipt marker field. The new recovery
fixture initially omitted its current-run settled tool checkpoint; it now
reconstructs that checkpoint from the retained typed call/result journal
rows, matching the compiler's deliberate omission of current-run items.
All original tool-history/no-repeat/cap/receipt assertions are retained.
Focused logs are `/tmp/ceilingdecl-{protocol,receipts,actor-laws,turnhygiene-regenerate,oneshot-regenerate}.log`;
command results are `/tmp/ceilingdecl-precheck-final-results.json`.

## Full gate evidence

All commands below use ENV LAW and prebuilt siblings, with fixture-update
variables removed. The first full workspace invocation was
`cargo test -q --workspace --no-fail-fast --locked` and finished in 1,148.50 s
with exit 101. Its sole failure was the unchanged
`tasks_runtime_tests::monitor_command_dispatch_waits_for_durable_broker_authorization`:
the marker existed but was still empty when the test expected `authorized`.
The test waits for file existence before reading; the shell command creates
the file before writing its contents. It exercises the task dispatcher
directly, without the new actor workspace capture. Running the exact current
compiled test alone passed in 0.48 s. No test, assertion, deadline, or ignore
was changed in response. The full workspace suite was rerun on the same source.

`cargo clippy --workspace --tests --locked -- -D warnings` passed in 270.76 s.
SHA-256 receipts of every changed/new crate input and `test-baseline.txt`
were identical before and after the first full test and Clippy invocations.
Logs: `/tmp/ceilingdecl-workspace.log`, `/tmp/ceilingdecl-monitor-isolated.log`,
`/tmp/ceilingdecl-clippy.log`; command statuses:
`/tmp/ceilingdecl-gate-results.json`.

The complete rerun of `cargo test -q --workspace --no-fail-fast --locked`
**passed**, exit 0, in 470.68 s. Its 334 emitted test-result records total
5,310 passed, 0 failed, and 13 existing ignored tests, including nested
subprocess tests; these are not the static test-count baseline of 4,904.
The large-history TUI probe passed; its existing debug-mode behavior does not
establish a release performance claim. No new ignore or platform gate was
introduced. Input hashes still matched after the rerun. Free space before the
rerun was 13,028 MiB. Evidence: `/tmp/ceilingdecl-workspace-rerun.log` and
`/tmp/ceilingdecl-workspace-rerun-results.json`.

## Final handoff

Independent verifier: **findings=4, real=4, noise=0**, all four code findings
fixed and covered: preserve cap/progress on receipt errors, count repeated
tool-call IDs, chunk durable receipts, and cancel receipt traversal. The
external upstream-ref advance is a workflow-state correction, not a fifth
code finding. No rejected finding is hidden as noise.

The 23-file deliverable is also exported to `/tmp/ceilingdecl.patch`, excluding
the supplied lane briefs and turnperf/turnperf2 evidence. The patch preserves
the local changes against the audited base; it does not include or substitute
for the pending upstream merge. No commit or push was completed.

**NO_SHIP:** the implementation passes its source review and local full gate,
but shared Git permissions prevent the requested merge and commit. The owner
must merge the advanced `origin/wave-970`, resolve the six overlapping files,
regenerate affected goldens through the tests, recheck the instruct-pipe pin,
recount the baseline, rerun the full gate and Clippy including tests, and then
commit without a trailer. The local green result is not a merged-tree gate.
