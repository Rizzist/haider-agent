# runperm continuation: resume parser semantic conflict

Date: 2026-09-05. Lane: `lane-970-runperm`; starting HEAD `2ef810ff`.
This continuation supersedes the earlier gate claims in `runperm.md` for the
merged parser. Commands and complete logs are recorded below.

## Root cause and contract decision

The landing log cites `run.rs:2209` correctly for the original failing tree.
The supplied log actually contains the same failure in **seven** targets
(`haider`, `cli_tests`, `observe_cli_tests`, `session_config_cli_tests`,
`update_cli_tests`, `update_restart_tests`, and `update_tests`), rather than five.
All seven reject resume with `--resume inherits the source session's model and
permissions`; none is a golden mismatch.

At `crates/haider-cli/src/run.rs:117`, new-run compatibility defaults are true
for writes, execution, and auto-allow. The resume guard previously inspected
those values, so every resume failed even when the caller supplied no permission
flags. It now checks the existing `allow_writes_seen`, `allow_exec_seen`, and
`auto_allow_seen` parser state, alongside explicit `read_only` and `trust_hooks`.

The two contracts coexist:

- A new run without permission flags remains write/exec/auto-allow capable.
- A resume handle, with or without any supported budget flag, parses without
  an additional prompt. The existing continuation prompt and budget handling
  are retained.
- Resume inherits the source session's model and permissions. Explicit
  permission flags remain rejected with the existing inheritance diagnostic.
  `--resume --read-only` is also rejected: accepting it would silently discard
  the requested restriction. This is an incompatible combination, with source
  permission inheritance taking precedence by returning a usage error.
- For a new run, `--read-only` continues to override the legacy allow flags in
  the existing permission policy. No permission policy or daemon code changed.

The client resume branch at `crates/haider-client/src/headless.rs:2955` reuses
`source.spec` and source session identity, merges explicit budgets/deadline/seed,
and does not apply the new-run request permission defaults to that session.
No journal, provider request, timing, or durability boundary changes here.
The turnperf and turnperf2 evidence was reviewed as historical context; this
continuation makes no latency claim or performance change.

## Merge-forward and per-file resolution

The required fetch/merge was attempted before edits. Both commands failed on
protected original worktree Git metadata (`FETCH_HEAD`, then `ORIG_HEAD.lock`).
A temporary checkout with its own writable Git directory successfully fetched
GitHub origin and ran the requested no-commit merge. Both fetched refs were
`b9c2a0475214102d1fb4c8d9c3ae3f480fd05fe4`, already included by HEAD; Git returned
`Already up to date.` Thus there are no new upstream file changes to reconcile.
See [merge command evidence](runperm-continuation/merge-forward.txt).

| File | Resolution |
| --- | --- |
| `crates/haider-cli/src/run.rs` | Semantic resolution: distinguish explicit flags from defaults; reject explicit read-only on resume; preserve both original lane tests and add two regression matrices. |
| `crates/haider-cli/tests/fixtures/turnhygiene/provider_request_no_budget.json` | Regenerated with `UPDATE_FIXTURES=1` through the existing provider ledger test: 1 passed; byte-for-byte unchanged. Never hand-merged. |
| `crates/haider-daemon/src/permissions_core_tests.rs` | No incoming conflict; full workspace gate validates the real instruct-pipe size: 12,764 → 12,764 bytes (no re-pin needed). |
| `test-baseline.txt` | Recounted with `xtask test-count --update`: 4,797 → 4,799; subsequent `test-count` confirms 4,799/4,799. |
| Lane briefs, landing log, `turnperf/`, `turnperf2/` | Supplied untracked context retained unchanged. |

No original index/ref/merge metadata was written and no commit was made. The
orchestrator owns recording the final tree; no pending new merge is necessary
for the fetched wave revision.

## Regression coverage

The original `resume_parser_accepts_handle_and_budget_without_prompt` test is
unchanged. New tests:

- `resume_parser_accepts_defaults_and_each_budget_without_prompt`: no budget,
  request tranche, max requests, max tokens, max cost, and max time; exact
  fallback prompt plus unchanged default permission projections.
- `resume_parser_rejects_explicit_permission_overrides_in_either_order`: each
  of read-only, allow-writes, allow-exec, auto-allow, and trust-hooks before or
  after resume; exact inheritance diagnostic.

Existing `run_parser_pins_outputs_timeouts_and_permission_flags`,
`run_write_and_exec_permission_flags_journal_ordinary_allow`, and
`run_read_only_denial_is_typed_and_terminal` retain their original assertions.
No test was deleted, weakened, ignored, or platform-gated.

## Gate environment and results

Every build/test/Clippy/count command exports the ENV LAW:
`RUST_MIN_STACK=8388608 HAIDER_DISCOVERY_DISABLED=1
HAIDER_TEST_DEVICE_NAME=test-mac CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0`.
`HAIDER_TEST_SIBLINGS_PREBUILT=1` is set only after successful sibling prebuild.
The built `haider` is 109,141,040 bytes and `haiderd` is 198,129,344 bytes;
the daemon exceeds the required 10 MiB sentinel.
Disk space is checked with `df -m /` before every build and gate.
All executions are on macOS; Linux/Windows behavior is by inspection only.

Completed before the full workspace gate:

- Sibling prebuild: exit 0; [log](runperm-continuation/prebuild.log).
- `cargo fmt --all -- --check`: exit 0; [log](runperm-continuation/fmt.log).
- `cargo test -q -p haider-cli resume_parser`: exit 0, 28 passed, 0 failed,
  0 ignored (the parser module is compiled into multiple targets);
  [log](runperm-continuation/parser-tests.log).
- `UPDATE_FIXTURES=1 cargo test -q -p haider-cli --test turnhygiene_pin_tests
  provider_request_body_is_budget_independent_and_matches_the_golden_ledger`:
  exit 0, 1 passed; fixture unchanged; [log](runperm-continuation/provider-golden.log).
- `cargo run --locked -p xtask -- test-count --update`, then
  `cargo run --locked -p xtask -- test-count`: both exit 0,
  4,799 tests / baseline 4,799; [log](runperm-continuation/test-count.log).

- `cargo test -q --workspace --no-fail-fast`: **exit 0**, 5,206 passed,
  0 failed, 13 ignored, 0 measured, 1,376 filtered across 329 reported harness
  summaries. These aggregate log totals include nested subprocess harnesses
  and doctests; the independent source-count baseline is 4,799. No command
  filter or skip was supplied to the workspace gate. The long viewport shape
  test completed successfully in 1,408.62 seconds.
  [Complete workspace log](runperm-continuation/workspace-tests.log),
  [exit/timing](runperm-continuation/workspace-status.txt),
  [machine-readable totals](runperm-continuation/workspace-totals.json).
- `cargo clippy --workspace --tests -- -D warnings`: **exit 0**, no warnings;
  [complete Clippy log](runperm-continuation/clippy.log),
  [exit/timing](runperm-continuation/clippy-status.txt).
- Final `git diff --check`: exit 0; `git ls-files -u` has no entries.

The workspace gate validates the existing instruct-pipe equality assertion at
12,764 bytes: old → real merged value is **12,764 → 12,764**. The full-prefix
macOS pin remains 18,572 bytes; inventory and 30% savings assertions are intact.
The provider golden also passed in the full gate without update mode.

## Independent verifier and final state

The independent verifier reviewed the completed parser diff and source-session
inheritance, confirmed both original contract tests remain intact, checked the
two new regression matrices, re-aggregated the full workspace log, and checked
Clippy's exit record. Final verdict: **SHIP**. A source citation was corrected
from the historical branch location to `headless.rs:2955`; this was citation
housekeeping and did not change code, tests, or verdict. There were no
substantive verifier findings or rejected findings.

The original HEAD remains `2ef810ff`. No original Git metadata or commit was
written. Tracked changes are only `run.rs` and `test-baseline.txt`; this evidence
and its command logs are additional untracked deliverables. Supplied briefs,
landing-failure log, and turnperf evidence remain unchanged.

VERIFIER: findings=0 real=0 noise=0 — no findings
SHIP

## Continuation CI registry walk

| Registry class | Continuation result |
| --- | --- |
| #1–#19 | Only manual parser validation and regression tests changed; no dependency, protocol, unsafe, or lint allowance changes. Formatting and whitespace checked. Final Clippy result recorded with the gate. |
| #20/#21/#48/#54 | Two new tests; original turnbudget and runperm assertions preserved. Counter updates 4,797 → 4,799. All Cargo test commands use the required 8 MiB stack. |
| #22–#44 | Resume source identity, persisted permissions, budgets, and journal semantics remain in the existing client/daemon path. |
| #45/#77 | No unsafe code or lint suppression added. |
| #46–#63 | Workspace containment, grant ceilings, process ownership, and platform behavior unchanged. |
| #64/#67/#71/#72/#74 | Fresh sibling binaries, daemon 198,129,344 bytes; discovery disabled and test device pinned. Original Git metadata restriction handled with a separate temporary Git directory; original tree remains uncommitted. |
| #65/#68–#78 | Explicit resume permission flags return the inheritance diagnostic; no silently ignored read-only request. Existing new-run typed-denial behavior unchanged. |
| #73 | Provider-request golden regenerated through tooling and remains byte-identical. Full workspace gate validates the unchanged instruct-pipe pin: 12,764 → 12,764 bytes. |
| #79–#93 | No process, output, line-ending, resource, or sampling changes. |
| #94/#95 | No new timeout, sleep, external-state wait, or keepalive behavior. |
| #96–#98 | No performance or provider/durability claim; turnperf evidence supplies historical context only. |

No new CI error class introduced; the defect is a semantic merge conflict
between a new default and an existing explicit-override check.

