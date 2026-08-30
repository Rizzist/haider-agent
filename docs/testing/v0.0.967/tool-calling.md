# Tool-calling end-to-end evidence

## Claim audit

The process-specific brief was audited before implementation:

| Brief claim | Verdict | Evidence |
|---|---|---|
| Branch base is `wave-967` at `83c6f68` | correct | `git rev-parse HEAD` returned `83c6f68`; no commit was made. |
| Foreground deliberately swept after leader exit and a unit test pinned exit → sweep → reap | correct at the base revision | `git show 83c6f68:crates/haider-tools/src/process.rs` calls `begin_group_termination` from the leader-exit arm; the base test is `shell_exit_sweeps_background_members_of_its_process_group`. |
| The recent real-RPC outliving-pipe test expected descendant output | correct at the base revision | The base `process_exec_drains_output_from_child_that_outlives_leader` expects `leaderchild`. It was updated in place, not deleted. |
| `HAIDER_DAEMON_TRACE=1` is defined at telemetry line 13, installed from `main.rs:120`, and limited to store/recovery | correct | `telemetry.rs:13-15` defines the env and two targets; `main.rs:120-121` installs it. |
| Actor command capacity is 64 at `session_hub/mod.rs:285` | value correct, citation drifted | The `Default` impl begins at line 285; the value is at line 288. |
| Deletion removes workflow, checkpoint, web-degrade, observe, and SSH state | correct; the earlier no-removal claim was wrong | `session_hub/mod.rs:5340-5361`. |
| `clear_session_surface` removes rather than inserts | correct; the earlier growth claim was wrong | Function at `session_hub/mod.rs:6263`, `remove` at line 6269. |
| `CountingAllocator` is at provider lines 354-371 | correct as a containing range | The test module begins at 354, the type is line 367, and the global allocator is lines 369-370. |
| Daemon library count is 828 passed / 3 ignored | correct | The clean unfiltered run executed 831 tests with exactly that result. The later parenthetical “must stay 826” in the brief is stale/wrong. |
| Fresh boot can overstate settled footprint by about 2.4× | not independently re-proven here | No fresh-boot number is used. Every retained measurement below settled for 10 seconds first and reports its snapshot uptime. |
| Roughly 60 GB was free | directionally correct at continuation start | Cargo preflights ranged from about 40 to 56 GiB free as cold final builds populated caches, always far above the 700 MiB stop threshold. |

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
  contains a 1 TiB sparse build artifact. Background-child output
  (`leaderchild`), no output, 512 KiB, and 2 MiB all reach spawn.
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

The sandbox denies external Mach task ports, so measurement used an
ad-hoc-signed copy of each production binary and an injected, inside-the-process
probe. The probe read `task_basic_info` RSS, `task_vm_info` physical footprint,
recursive VM-region dirtied/swapped/resident pages, DefaultMallocZone
statistics, and an enumerator-derived allocation size-class histogram. All Mach
and allocator probe return codes were zero. The page size was 16,384 bytes.

Each binary started a real in-process daemon with a fake provider and executed
`/bin/echo trivial` through the real RPC `process_exec` route. Each snapshot
settled 10 seconds before the command and 2 seconds after it. Three baseline/fix
pairs were interleaved; matched-state deltas cancel the probe and durable-turn
overhead common to both builds.

| Median of three before → after deltas | Base `83c6f68` | Fixed | Fixed − base |
|---|---:|---:|---:|
| RPC command latency | 108.519458 ms | 134.617834 ms | +26.098376 ms |
| RSS (`task_basic_info`) | +8,929,280 B | +10,469,376 B | +1,540,096 B |
| Physical footprint (`task_vm_info`) | +4,292,632 B | +4,309,016 B | +16,384 B |
| Resident VM pages | +350 | +726 | +376 pages |
| Dirty VM pages | −9 | +81 | +90 pages |
| Swapped VM pages | +268 | +168 | −100 pages |
| Live malloc blocks | +9,073 | +9,080 | +7 |
| Malloc bytes in use | +1,667,760 B | +1,669,424 B | +1,664 B |
| Malloc bytes reserved | +0 | +4,194,304 B | +4 MiB quantized/noisy |

Baseline command latencies were 108.519458, 157.242208, and 107.049500 ms;
fixed latencies were 134.617834, 108.338042, and 190.423875 ms. Every number in
the table is a matched delta between these exact snapshot-uptime intervals:
baseline 28.407375→40.848125 s, 17.958865→28.348962 s, and
16.548406→23.762488 s; fixed 23.964772→36.276736 s,
15.636606→24.927155 s, and 16.473808→23.769296 s. Thus even the earliest
snapshot was settled for more than 15.6 seconds of process uptime; no
fresh-boot footprint is reported.

The median histogram deltas (base/fixed) were: `<=16 B` 6,010/6,011,
`<=32 B` 367/367, `<=64 B` 460/463, `<=128 B` 201/203, `<=256 B` 58/58,
`<=512 B` 205/204, `<=1024 B` 1,708/1,710, `<=2048 B` 36/36,
`<=4096 B` 11/11, `<=8192 B` 14/14, and larger 3/3. VM residency,
dirtiness, swapping, and malloc reservation were visibly noisy and quantized.

This measurement does **not** establish a latency or memory improvement: with
three noisy end-to-end samples, the fixed median latency was higher and the
physical-footprint delta was effectively one 16 KiB page higher. It does settle
the required before/after cost without an unmeasured claim. The structural
guarantee comes from the normal-completion branch and its pins: it never calls
the group-termination ladder, never creates the two-second grace timer, stops
the output readers, relinquishes the exact group authority, and lets the
supervisor finish. The nonzero deltas include the real provider/RPC turn,
journal, projection, and durable session state; this harness cannot attribute
those allocations specifically to `/bin/echo`.

Mutation evidence was executed, not inferred. With a temporary mutation at the
production `RegisteredToolRoute::ProcessExec` arm, the first scenario failed at
the missing continuation marker. With direct `shell.exec` temporarily replaced
by `exit 97`, the shell scenario failed at the expected stdout byte assertion.
Both mutations were removed before the clean run.

Not yet proven locally: Windows/Linux process-tree behavior. The commands have
platform-specific fixtures and compile in their cfg arms, but only the CI legs
can execute those kernels. Provider prose quality and vendor-side cache billing
remain intentionally untested.
