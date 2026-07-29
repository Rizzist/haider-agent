# W5b review — round 3 (close-out confirm of W5b.1b)

- Frozen SHA: `e18ef3e` (branch `w5-b`). Method: dual review — gpt-5.6 (xhigh, subagents) + Fable independent confirmation. My independent socket gate: 999 pass, real UDS RPC + all 5 new pins green, baseline 988.

## Per-P1 status

- **P1-2 publish-before-commit — CLOSED.** Public + pre-admitted resolvers cannot return generation N+1 before durable descriptor restoration or joined fail-closed deletion (oauth.rs:1985). Panic waiters wake; failures release closed. Confirmed.
- **P1-1 barrier transitive closure — STILL-OPEN (narrowed to the error path).** The normal deadline-forced path is now correct: `actor.force_and_join()` (runtime.rs:570) joins the exact blocking vault write, zeroizes owners, withholds `Stopped`/lease, no internal deadlock; a permanently hung vault intentionally wedges shutdown fail-safe. Residual below.

## Blocking finding (VERDICT: NO_SHIP)

- **[P1] worker-manager-error early-return bypasses the barrier cleanup** — `runtime.rs:533` (`Some(Err(error)) => return Err(error.into())`) fires BEFORE the broker cleanup (runtime.rs:540), oauth cleanup (550), and the account-actor `force_and_join` (560-571). If `worker_manager.shutdown()` errors while the account actor's refresh persistence is blocked in `spawn_blocking`, the actor handle drops (task abort only — the blocking vault write survives) and the profile lease releases → a successor daemon can race the surviving write. accounts.rs:583-587. Fable-confirmed at runtime.rs:529-571. Fix: run the barrier cleanup UNCONDITIONALLY — a worker-manager shutdown error must not skip the broker/oauth/account-actor join + lease withholding; capture the error, complete the barrier (join the blocking persistence, decide the lease), then propagate.

## Non-blocking (fix in the same round)

- **[P2] flight registered before cancellable task** — `oauth.rs:2057-2082`. Flight registration precedes cancellable-task registration; a cancellation while `OwnedTaskSet::spawn` yields leaves an incomplete flight permanently mapped → later resolvers wait forever and graceful shutdown does not poison it. Fix: register the task before/atomically with the flight, or poison a flight that has no live task.
- **[P2] panic cleanup precedes admission seal** — `oauth.rs:64-100,1913-1918`. Panic cleanup removes/wakes the flight before the `JoinError` seals task admission; a concurrent resolver can register replacement refresh work during the unwind window. Fix: seal admission before removing/waking on panic.
- **[P3] commit-gate pin misses outer-admission-only revert** — `oauth_tests.rs:1722-1855`. The pin kills inner/pre-admitted gate removal and the combined revert, but an outer-admission-only revert is masked by the still-present inner check. Fix: pin the outer admission gate independently, or document it as redundant defense-in-depth with the inner as load-bearing.

## Audit integrity (r3 read)

- P2-4 secret sweep: load-bearing (renders the real TUI; live HTML checks actual nonce + callback-path).
- `concurrent_resolve_waits_for_rotated_descriptor_commit_or_fail_closed_delete`: load-bearing for INV-1 / inner gate; partial for the redundant outer admission (P3).

## Required for W5b.1c SHIP

Fix the P1 (unconditional barrier cleanup on worker-manager error) + both P2 + the P3 pin. Re-run the FULL mutation audit including a NEW pin: a worker-manager-error injected during blocked refresh persistence must leave NO surviving vault write / released lease for a successor (revert of the unconditional-cleanup fix must kill it). Socket-capable gate. Review r4 focuses the P1 error-path closure + the two P2 ordering fixes.
