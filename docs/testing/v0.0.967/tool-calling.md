# Tool-calling end-to-end evidence

## Claim audit

The process-specific brief was re-audited on the merged tree:

| Brief claim | Verdict | Evidence |
|---|---|---|
| P1 starts at `652d9ea`, rebased on `61c1ab3`, with 11 files / 636 insertions | correct | The merge base is exactly `61c1ab3`; `git diff --shortstat 61c1ab3 652d9ea` reports 11 files, 636 insertions, 156 deletions. No commit was made during verification. |
| The rebased core-loop body matched the P1 comment | wrong | Its name/comment had P1 semantics but the assertion still required the descendant marker to be absent. The body now requires survival and `leader`-only output; the full binary passes 10/10. |
| Lane 966-T1 is `2e5b365` | correct | The commit exists and is titled “process_exec read the whole workspace before it would run anything.” |
| W1's bounded receipt is already in the base | correct | `bde0332` is an ancestor and owns `workspace_receipt.rs`; P1 uses `finish_for_root` and adds no parallel receipt implementation. |
| A non-repository receipt costs exactly one stat | wrong literally | `detect_repo_root(root, root)` checks `start.is_dir()` and then metadata for `.git`. The important contract is correct: it returns `NotEnumerated`, with zero walked entries/content, and every unknown comparison reports `assumed_mutation=true`. |
| `HAIDER_DAEMON_TRACE=1` is at telemetry line 13, installed from `main.rs:120`, and limited to store/recovery | citation drifted | `telemetry.rs:13-15` is exact. `main.rs:120` is the containing function; the install call is line 121. |
| `CountingAllocator` is at provider lines 354-371 | correct as a containing range | The test module begins at 354, the type is line 367, and the global allocator is lines 369-370. |
| Daemon library count is 828 passed / 3 ignored | drifted | W1 added five tests; the merged tree runs 833 passed / 3 ignored. |
| RPC goldens are 180 frames | drifted | The v0.0.966 prefix is 180; current K1 adds two, and the fixture/pin are 182. The crate's 154 tests pass. |
| Journal commits issue `F_FULLFSYNC` | wrong | The event store uses WAL and configured `synchronous=NORMAL` by default; its own documentation explicitly notes that SQLite `FULL` still does not mean `F_FULLFSYNC`. |
| `queue_wait_micros=269` measures mutex contention | wrong | The field is recorded around blocking-pool scheduling in `sqlite_store.rs`; it is not a mutex-wait metric. |
| The armed process-exit path still polls at 1 kHz | wrong | Linux uses pidfd and macOS uses kqueue; only unsupported targets retain the bounded one-millisecond fallback. |
| Fresh boot overstates settled footprint by about 2.4× | not re-proven | The retained measurement waits 10 seconds before every command, so no fresh-boot value is used. |

## Blast radius and replacement design

Every invocation still creates its own containment coordinate. Unix sets process
group zero before spawn (`haider-platform/src/process.rs:481-485`), making the
leader PID the fresh PGID, and teardown calls `killpg` only for that
`ProcessGroup` (`process.rs:926-930`). Windows creates a fresh suspended process,
registers it in one uniquely tokened Job Object, and terminates only that Job
Object (`process.rs:273-332,1231-1260`). The cancellation test additionally
keeps an unrelated process alive while the owned group dies. The new blast
radius is therefore no wider than the old one: exactly the group/job created by
this invocation. A descendant that deliberately creates a new session or
process group remains the already-documented containment escape.

Natural completion now follows a different path:

1. Close live `process_control` authority while the observed zombie still pins
   the Unix PGID, then reap the leader (`haider-tools/src/process.rs:1374-1414`).
2. Give pipe reads already ready in that scheduler turn one chance to publish,
   without a timer, close inherited output, and detach rather than terminate
   (`process.rs:1415-1429`).
3. On Unix detach is a no-op. On Windows it clears
   `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` before removing the exact unique Job
   token (`haider-platform/src/process.rs:813-846`).
4. Background supervision uses the same natural-completion boundary
   (`haider-tools/src/tasks.rs:494-502,627-680`). A live `task_kill` still runs
   the group kill ladder.

There is one explicit exceptional residual. If Windows refuses to clear
`KILL_ON_JOB_CLOSE` after a normal leader exit, closing the Job handle would
itself sweep the descendants. That error path therefore reports a typed runtime
fault, removes both lookup coordinates, and deliberately abandons the one exact
Job handle instead of converting completion into teardown
(`haider-platform/src/process.rs:848-872`). Descendants survive while the daemon
process remains alive; when the OS ultimately closes the abandoned handle at
daemon-process exit, the fail-closed Job policy can terminate them. This is not
the successful-detachment contract described in the model manual, and it is
not silently represented as one.

Cancellation, the 60-second wall bound, the 1 MiB combined-output bound,
explicit teardown, and a foreground supervision failure still call the existing
TERM → two-second grace → KILL ladder (`haider-tools/src/process.rs:1151-1366,
1854-1902`). Retaining the sweep for supervision faults is the narrow safety
exception: returning while this invocation's command is still live would orphan
it merely because capture, output publication, spill, or CAS supervision failed.

The removed guarantee is not replaced with hidden ownership. After a normally
completed foreground or background leader leaves descendants, the terminal
registry no longer has a live kill coordinate for them; daemon shutdown does
not reclaim them. Nobody in Haider owns them. That is the explicit owner-directed
contract, and the model-facing manual says so.

The released `v0.0.966` history contains both process regressions named in the
brief. Commit `2e5b365` records that `process_exec` synchronously read the whole
workspace before spawn and completion; commit `c6fa6d7` replaced that receipt
with the bounded implementation now in
`crates/haider-tools/src/workspace_receipt.rs:20-25`. The current receipt uses
Git status for repositories, bounds traversal to 4,096 entries, bounds content
to 16 MiB, and bounds wall time to 500 ms. A non-repository is deliberately not
enumerated and receives typed unknown coverage
(`workspace_receipt.rs:28-84`).

The existing `live_turn_rpc_tests` does exercise real processes over the
production RPC transport, including direct shell execution and cancellation,
but the large ignored-directory and non-repository 966 shapes are covered only
below the daemon in `haider-tools`. That gap explains why the released core
loop could still hang while lower-level suites were green.

## Deterministic design

The new `core_loop_e2e_tests` binary starts `haider_daemon::spawn_with_dependencies`
through the shared `haider-daemond/tests/support` harness and connects with a
real platform IPC stream, handshake, wire codec, request dispatcher, worker,
tool registry, permission broker, OS process supervisor, journal, projection,
and RPC readback. The only substitute is `ProviderFactory`: `FakeProvider`
scripts deterministic tool calls and explicitly requires each real tool result
before emitting a continuation marker. No tool dispatcher, broker, filesystem,
process, store, projection, or client frame is mocked.

The suite covers real `process_exec` output for background/descendant and
pipe-held shapes, no output, large output, the hard output cap, and cancellation;
it will run in both a Git workspace with a large ignored build directory and a
non-repository root. It will also cover real `fs_read`, `fs_write`, `fs_edit`,
`fs_search`, and receipt-backed `shell.exec` (the `!` path). A timeout is only
the test harness's failure bound; success requires the expected OS output or
filesystem mutation and a continued provider turn.

No live model judgment is needed or claimed. Provider prose quality and a
provider vendor's own cache billing cannot be tested deterministically here.

## Results

Implemented in `crates/haider-daemond/tests/core_loop_e2e_tests.rs` and run on
macOS with the release-gate environment. The following real-daemon scenarios
pass:

- `tool_calls_execute_and_continue_over_real_rpc`: an ignored `target/`
  contains a 1 TiB sparse build artifact. Foreground leader output (with
  race-permitted descendant bytes), no output, 512 KiB, and 2 MiB all reach
  spawn.
  The 2 MiB command returns the typed 1 MiB output-cap failure. `fs_write`,
  `fs_read`, `fs_edit`, and `fs_search` mutate/read the actual workspace, and
  every result is consumed by a second provider request before the run reaches
  `Done`.
- `process_exec_runs_in_a_non_repository_workspace`: a plain directory returns
  the actual `nonrepo-real-output` bytes and the turn continues. It does not merely
  return a timeout or unknown-coverage error.
- `direct_shell_rpc_executes_and_is_visible_to_the_next_turn`: `shell.exec`
  returns `shell-real-output`, creates `shell.txt`, commits `Done`, and the next
  provider request contains the raw user-command record.
- `cancelling_process_exec_kills_the_real_process_group`: both the leader
  heartbeat and an already-started descendant stop after `turn.cancel`; the
  descendant's delayed survival file never appears.

Lane 967-P1 deliberately supersedes the original outliving-pipe expectation.
`process_exec_normal_completion_leaves_outliving_descendant_alone` now pins the
owner's completion boundary: the foreground result contains `leader`, closes
its inherited output readers promptly, and leaves the descendant alive long
enough to write its durable marker. Late `child` bytes are not foreground
output. Long-running work that needs owned output and a kill handle uses
`background=true`.

## Trivial-command cost, before and after

The sandbox denies external Mach task ports, so the merged-base measurement
used one identical integration probe linked separately against `61c1ab3` and
the fixed tree. Both 9.8 MiB executables were ad-hoc signed and read their own
`task_basic_info` RSS and `task_vm_info` physical footprint. All Mach return
codes were zero. Each process settled for 10 seconds, then ran the machine-native
command `ls` through `EffectBroker::process_exec` in an empty non-repository
workspace. Five base/fix pairs were interleaved.

| Median of five before → after deltas | Merged base `61c1ab3` | Fixed | Fixed − base |
|---|---:|---:|---:|
| `ls` process-exec latency | 18.761208 ms | 16.774792 ms | −1.986416 ms |
| RSS (`task_basic_info`) | +1,409,024 B | +1,441,792 B | +32,768 B |
| Physical footprint (`task_vm_info`) | +311,296 B | +294,936 B | −16,360 B |
| Group-sweep lifecycle events | 1 start + 1 complete | 0 + 0 | −2 events |
| Normal-completion detach event | 0 | 1 | +1 event |
| Workspace entries/content read | 0 / 0 B | 0 / 0 B | unchanged |

Base latency samples were 21.263583, 18.138042, 17.306291, 19.184792, and
18.761208 ms. Fixed samples were 18.680500, 12.804833, 22.429125, 13.755083,
and 16.774792 ms. Every base run entered `GroupSweepStarted` and
`GroupSweepCompleted`; every fixed run entered neither and instead recorded
`NormalCompletionDetached`. Every receipt was W1's
`not_enumerated_non_repository` with `assumed_mutation=true`, known counters,
zero before/after entries, and zero before/after content bytes. Thus P1 adds no
second receipt implementation and does not enumerate the workspace.

This measurement does **not** establish a latency or memory improvement: the
fixed median was 1.99 ms faster, while its median RSS delta was two 16 KiB
pages higher and physical footprint about one page lower. Those are small and noisy
null results, reported rather than optimized away. The measured structural
result is the directive: `ls` performs no process-group teardown and arms no
teardown grace path on natural completion. W1, not P1, owns the separate
receipt cost.

Mutation evidence was executed, not inferred. With a temporary mutation at the
production `RegisteredToolRoute::ProcessExec` arm, the first scenario failed at
the missing continuation marker. With direct `shell.exec` temporarily replaced
by `exit 97`, the shell scenario failed at the expected stdout byte assertion.
Both mutations were removed before the clean run.

Not yet proven locally: Windows/Linux process-tree behavior. The commands have
platform-specific fixtures and compile in their cfg arms, but only the CI legs
can execute those kernels. Provider prose quality and vendor-side cache billing
remain intentionally untested.
