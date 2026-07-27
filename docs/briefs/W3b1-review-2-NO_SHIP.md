# W3b1 review round 2 — NO_SHIP

- Reviewer: gpt-5.6, 2026-07-27. Frozen SHA 93f42a9 (scope 1183fb4..93f42a9).
- CLOSED: P2-4 reason-halving + NoticeUndelivered-forces-outcome; P2-5 additive MenuAnswer
  .request_id (uncorrelated golden bytes verified unchanged); P2-6 ConnectionGrant retains AND
  enforces the capability set; P2-7 handshake_timeout; P3-8 release-build phase checking.
- R-compliance: R1/R2/R4/R12/R16/R18 COMPLY; R3, R17, R22 still VIOLATE.
- Full log: ~/haider-run/w3b1-review-r2.log

## Findings (round 3)

P1-1 (was PARTIAL) — **the nested writer is aborted but never JOINED** (connection.rs:71,
runtime.rs:372). `finish` correctly retains the handle and the writer adopts the deadline
mid-frame, but cancellation only calls `abort()` and drops the child handle; the runtime joins
only OUTER connection tasks. Tokio abort is asynchronous, so the writer future — and its
OwnedWriteHalf and payload — can still be alive when endpoint cleanup and store close run.
Fix: own the child writer's completion (join it, with the barrier deadline) before teardown
proceeds. Accepted as unavoidable: an ordinary frame already partly on the wire leaves a
truncated prefix then EOF — the reviewer confirms the notice is NOT spliced behind it.

P1-2 (was PARTIAL) — **finalization can still report Graceful after overrun/second signal**
(runtime.rs:459/467/420/395/409). `bounded_finalization` accepts a first-poll `Ready` BEFORE
inspecting an expired deadline or force; with work and force/deadline simultaneously ready an
unbiased `select!` may pick work; socket cleanup is synchronous OUTSIDE the select so it can
never observe a second signal; and no final recheck precedes reporting `Graceful`.
The reviewer AGREES the `timeout(ZERO, …)` objection was valid — the replacement just needs
POST-POLL ARBITRATION: after each step completes, re-check deadline/force and downgrade the
outcome; bring cleanup inside the same discipline; recheck once more before reporting Graceful.

P1-3 (NOT_FIXED) — **the "private" staging names are enumerable and bind/probes stay path-based**
(endpoint.rs:197/230/139/157/343/401/425/438/458/463/269). The check→use split simply moved under
`.tXXXXXXXX` names whose 32 bits a same-UID process can enumerate: a racer can replace staging
before `statat`, replace a claimed node between `statat` and `unlinkat`, or replace a refused
stale claim before unlink. `restore` is additionally a REPLACING rename that ignores failure —
a crash strands the claimed live/foreign node and a third public node can be overwritten during
restore. Public-name publication IS safe on this macOS host via RENAME_EXCL, but the cfg fallback
(endpoint.rs:269) explicitly retains a replacing race on other Unix targets.
Fix direction: make the private names UNGUESSABLE (≥128 bits from a CSPRNG) so claim→verify→unlink
cannot be targeted; make `restore` non-replacing and handle its failure; sweep stranded `.t*`
entries at startup; state the irreducible residual precisely. NOTE the reviewer's own observation:
the store lifetime lock already prevents a normally-starting peer daemon from reaching bind — the
residual threat is a same-UID process deliberately racing, not accidental successor deletion.

P3-4 — **several claimed regression tests do not discriminate** (lifecycle_tests.rs:1117,
state_machine_tests.rs:26): the endpoint "race" test requests shutdown then replaces the path
synchronously on a current-thread runtime without yielding, so replacement ALWAYS precedes cleanup
and the OLD TOCTOU implementation would also pass; the phase test checks only `can_transition_to`,
never that `publish` refuses an illegal edge. Deferring raw EAGAIN/EPIPE is honest (no hook), but
deferring second-signal finalization is NOT — `bounded_finalization` can be driven deterministically
with synthetic ready/pending futures and a watch channel.
Also still uncovered: a literally-never-reading client (the current test reads 8 KiB), child-writer
join completion, zero/one-byte readers.

VERDICT: NO_SHIP — writer completion is not owned, finalization can miss the deadline or second
signal, and endpoint ownership remains raceable; W3b2 cannot safely attach yet.
