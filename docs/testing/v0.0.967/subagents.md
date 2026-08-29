# Subagent end-to-end evidence

## Claim audit

The typed tool schema carries independent optional `model` and `provider`
fields in `crates/haider-tools/src/spawn_subagent.rs:22-28`, and the daemon
resolves them before durable child creation in
`crates/haider-daemon/src/worker.rs:13567-13712`. Fleet nodes carry persisted
`callsign`, `model`, `provider`, and task identity in
`crates/haider-rpc/src/frame.rs:2140-2179`; commit `2122c96` is the 966 X1 change
that added model/provider delivery.

The reported completion defect is present in the release. Collection observes
a stored or derived report and then unconditionally calls
`mirror_until_child_terminal` (`delegation.rs:777-806`). That loop exits only
after a terminal Idle fence and a non-live metrics snapshot
(`delegation.rs:2046-2064`); it has no cancellation branch, timeout, or repair
for a terminal run whose Idle settlement is absent. The same wait is also used
after parent cancellation (`delegation.rs:830-838`).

## Deterministic design

The same production-daemon/real-IPC boundary as the tool suite is used. A
routing fixture provider selects scripts from the session's persisted provider
and model, which lets the suite prove same-provider and cross-provider child
execution without any network call. Parent and child both traverse the real
worker, delegation store, child session, provider resolution, deferred tool
collection, parent continuation, journal, fleet projection, and RPC surface.

Scenarios are: normal child completion returned to the parent; terminal child
without Idle; provider-resolution failure while launching a child; explicit
cross-provider spawn; manual cancellation of a running child; and fleet
identity containing task/callsign, model, and provider. The missing-Idle state
uses no store injection: while the first real child run is active, the RPC
client queues a second turn to that child. Run one then reaches `Done` while
run two starts, deliberately preventing an aggregate child-session `Idle`.

No live-model assessment is claimed. Whether a child wrote a *good* report is
outside this deterministic suite; whether the report returns and the parent
continues is fully automatable.

## Results

Implemented in `crates/haider-daemond/tests/core_loop_e2e_tests.rs`.

- `cross_provider_subagent_returns_to_parent_and_fleet_is_truthful` passes.
  The parent executes on `parent-provider/parent-model`, the child executes on
  `child-provider/child-model`, the child report returns through the real
  deferred tool result, and the parent reaches `Done`. `session.fleet` reports
  a non-empty callsign plus exact task, model, provider, and `done` state.
- `subagent_launch_failure_returns_to_parent_instead_of_hanging` passes. A
  factory-resolution failure makes the child `failed`, returns a typed result,
  and the parent continues.
- `manually_cancelled_running_child_releases_parent` passes. The client obtains
  the child's durable run coordinates through a real child attachment, sends
  `turn.cancel`, observes fleet `cancelled`, and the parent continues.
- `terminal_child_run_without_session_idle_still_releases_parent` fails. The
  queued second child run proves run one has terminalized without aggregate
  `Idle`; the parent emits no continuation. The regression is run in an
  isolated copy of the test executable and is killed after eight seconds, so
  this failure can never recreate the released infinite CI hang.

This is a production defect, not a fixture timeout: normal completion,
resolution failure, and manual cancellation all settle through the same
daemon and RPC harness. The stuck path is the unconditional
`mirror_until_child_terminal` call after a derived terminal report, whose loop
requires `terminal_idle_seen` and has no cancellation/repair branch. The
concurrent child-lifecycle lane owns `delegation.rs`, so this lane reports the
defect and does not edit that file.

Mutation evidence was also executed: temporarily rejecting the production
`SpawnSubagent` dispatch made the cross-provider test fail at the missing
parent continuation. The mutation was removed. The unmodified missing-Idle
test itself is the stronger regression demonstration.

Live-model report quality is not claimed. Windows/Linux execution remains for
their CI kernels; the local macOS behavior above is fully exercised.
