# W6c — stall supervision + depth-capped recursion

AUTHORITY: docs/research/w6-subagent-research.md (Q1's W6c section) and
the SHIPPED W6a structures (delegation.rs coordinator, schema v9
delegations table, deferred spawn tool, ChildWaitCheckpoint).

## Scope

1. **Stall supervision.** A child that makes NO progress (no new
   envelope committed on its session) for a deadline (default 120s,
   const) gets ONE nudge — a daemon-authored steer turn ("report your
   status or conclude") through the existing internal-turn machinery;
   if a further deadline passes with no progress, the child is
   CANCELLED through the existing cancel path and its terminal report
   carries the stall reason. The parent resumes normally with that
   report (the W6a collect loop already handles terminal children).
   Supervision is coordinator-owned; deadlines survive daemon restart
   (derive from the delegations table + the child's last committed
   envelope time — no new table unless genuinely needed).
2. **Depth-capped recursion.** A CHILD may call `spawn_subagent`: the
   manifest's `depth` increments from the parent's delegation row; the
   cap is 3 (const). At the cap the tool returns a TYPED tool error
   ("recursion depth limit") the model can read — never a run failure.
   Ancestry (root_session_id, parent_agent_id) chains correctly; the
   research's warning stands: do NOT inherit the sim's nested-spawn
   dead-end.
3. **Child cancel-on-parent-cancel.** Cancelling the parent's turn
   cancels its outstanding children (the W6a collect loop already exits
   on parent cancel; the children must not be orphaned — the
   coordinator sweeps them through the cancel path).

OUT: chip steer/close/answer RPCs (a later UI-control patch), remote
placement, budget enforcement.

## Laws

Same as W6a: tests never inline; mutation docs with runtime failures;
`CARGO_INCREMENTAL=0`; fmt + workspace clippy clean; test the affected
crates (`haider-daemon`, `haider-core`, `haider-tools`,
`haider-protocol` if touched); ledger update; no haider-tui, no
Cargo.lock, no versions; leave changes uncommitted; no git commands.
Additive protocol only.

## Tests (minimum)

- A stalled child gets exactly one nudge, then cancel; the parent
  resumes with a stall-reason report (mutation: drop the nudge step →
  the one-nudge pin fails; mutation: drop the cancel → the resume pin
  times out/fails).
- Progress resets the deadline (a slow-but-alive child is never nudged)
  (mutation: deadline ignores progress → fails).
- A depth-2 spawn works and chains ancestry; a depth-4 spawn returns
  the typed tool error and the parent turn CONTINUES (mutation: cap
  removed → the cap pin fails; mutation: cap error becomes a run
  failure → the continues pin fails).
- Parent cancel sweeps the children (mutation: sweep dropped → orphan
  pin fails).
- Restart mid-wait re-arms supervision from durable state (mutation:
  supervision only armed at spawn time → the restart pin fails).

Use up to 2 research subagents and 2 verify subagents. Print a final
summary of files changed and tests added.
