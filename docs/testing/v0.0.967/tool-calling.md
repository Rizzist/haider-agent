# Tool-calling end-to-end evidence

## Claim audit

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

`process_exec_drains_output_from_child_that_outlives_leader` fails against the
release implementation. An initial diagnostic command returned only `leader`,
not the later `child` bytes. The final pin also makes the background child write
a durable marker before its stdout bytes; production terminates that child as
soon as the shell leader exits, so the marker is absent. This is a clean
premature-completion failure, not a timeout converted into success.

The process implementation is owned by the concurrent lane, and this brief
also forbids editing `haider-tools/src/process.rs`, so this lane leaves the red
pin and reports the defect.

Mutation evidence was executed, not inferred. With a temporary mutation at the
production `RegisteredToolRoute::ProcessExec` arm, the first scenario failed at
the missing continuation marker. With direct `shell.exec` temporarily replaced
by `exit 97`, the shell scenario failed at the expected stdout byte assertion.
Both mutations were removed before the clean run.

Not yet proven locally: Windows/Linux process-tree behavior. The commands have
platform-specific fixtures and compile in their cfg arms, but only the CI legs
can execute those kernels. Provider prose quality and vendor-side cache billing
remain intentionally untested.
