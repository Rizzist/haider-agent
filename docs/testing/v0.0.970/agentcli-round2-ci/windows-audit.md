# Windows CI claim audit (read-only)

Executed inspection: `gh run view 33980468392 --job 101344667648 --log` and `gh run view 33980468392 --job 101344667770 --log`. No code edits or builds performed by this researcher. Full logs remain at /tmp/agentcli-round2/windows-test.log and /tmp/agentcli-round2/windows-clippy.log; the main report links the GitHub jobs.

## Claims

- discovery.rs:125 is correct on the starting tree: `with_request_observer_for_test` is emitted for `cfg(test)` at line 124. Its only callers are update/tui_tests.rs:82 and :192; that file has `#![cfg(unix)]` at :2 and is loaded under `cfg(test)` by tui.rs:136-138. Correct method gate is `#[cfg(all(test, unix))]`, with no allow added. Clippy log :418-430 contains exactly this compiler error and failure.
- Windows test job has FIVE test failures, each identical to an existing xplatfix-owned failure, not six. The six-failure ownership brief is `/Users/rizzist/.claude/jobs/1a0b5b6c/tmp/970-xplatfix-cont-brief.md` (earlier run 33972289990 / job 101322816225).

| Existing brief item | Current job result | CI source citation | Log line |
|---|---|---|---|
| 1 instruct_pipe_shrinks_the_advertised_wire_pack | FAIL: 5667 actual vs 5670 pin | permissions_core_tests.rs:1570 | 3209-3212 |
| 2 verified_slot_resolves_journal_provenance_with_slim_live_and_replayed_results | FAIL: guard opens abandonment confirmation: Elapsed(()) | g1_todo_runtime_tests.rs:235 | 3216-3217 |
| 3 native_pipe_io_failure_never_fails_the_journal_append | PASS | session_hub_tests.rs (test name searched) | 3280 |
| 4 provider_request_body_is_budget_independent_and_matches_the_golden_ledger | FAIL: only JSON difference is tools[7].function.parameters.properties.command.description, shell vs PowerShell | turnhygiene_pin_tests.rs:446 | 6834-6838 |
| 5 resident_daemon_discovers_a_hook_installed_between_runs_and_scopes_it_by_cwd | FAIL: capture file never reached 2 lines (has 0) | turnhygiene_pin_tests.rs:657 | 6844-6845 |
| 6 monitor_cwd_ancestor_cannot_be_replaced_between_prepare_and_spawn | FAIL: spawned process must retain prepared cwd | process.rs:2530 | 7542-7543 |

`git diff --name-only f211be0e^1 f211be0e -- crates` excludes all four failing source files. The exact 5 failures already existed in the xplatfix brief. The new agent_cli_tests suite ran all 12 tests successfully (windows-test.log:6412-6428). No agentcli-owned Windows test failure found.

The platform observer claim citation process_tests.rs:246 is correct on the starting tree: an absolute 50ms Tokio timeout around public observer. The child is /bin/sleep 0.005 (:234-237), so neither delayed scheduling nor actual arming mechanism is isolated.

Relevant read-only lens evidence: turnperf2/C2.md:8 identifies event-driven scheduling replacements and residual coarse backoffs as mechanisms, not clean-median budget claims; C3.md:4 requires joins before runtime disposal, :7 retains exact spawned-child identity, :11/:13 distinguishes event-driven completion and 0ms happy-path sleeps from watchdog allowances. D4.md distinguishes measured evidence from estimates and preserves persistence/publication authority. No additional performance work proposed.

## Independent deterministic test assessment

Use /bin/cat with piped stdin and no output. Capture retained identity and poll public observer plus retained.wait once while stdin is still held, require both Pending, then drop stdin. Under a paused current-thread Tokio clock, keep the test runnable with poll_fn self-wake while checking both pinned futures; never repoll one that already returned Ready. A Tokio sleep polling replacement cannot progress because virtual time remains unchanged. Kernel kevent threads can deliver their oneshots. Anchor a real Instant before initial polls; require completion before NOTIFICATION_REPAIR_INTERVAL/2 (15s versus earliest 30s native repair) so no repair poll can supply a pass. Check the real deadline before accepting Ready; assert virtual Instant unchanged; cleanup the owned child and observers before asserting an error. Move the existing const to module scope for the test to derive its watchdog without changing production behavior.
