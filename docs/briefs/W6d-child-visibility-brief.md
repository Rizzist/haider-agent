# W6d — child visibility: bubbled asks, honest chip states, no false stalls

OWNER BUG (journal autopsy, session-child-d6c6b1c4…): a delegated child
called process_exec, hit the W8 Ask default, parked in
`permission_required` (seq 72) — INVISIBLE (chip badge stuck "thinking",
chip view empty) and UNANSWERABLE. The parent waited forever. Claude
Code's contract is the inspiration: a subagent's asks reach the user;
its transcript (including the delegated prompt) is visible.

## Scope (daemon/core/protocol — NO haider-tui; the TUI lane follows)

1. **Chip-state truth.** The delegation coordinator mirrors the CHILD's
   run-state transitions into the parent's `AgentChipState` events —
   additive `ChipState` variants as needed (at minimum:
   InputRequired, PermissionRequired; map Waiting/Compacting/etc. to
   Thinking). The parent's journal therefore names what the child is
   doing; no more permanent "thinking".
2. **No false stalls.** Stall supervision (delegation.rs) treats a child
   parked in InputRequired/PermissionRequired as WAITING ON A HUMAN:
   the stall clock pauses while parked (no nudge, no cancel). It
   resumes when the child leaves the parked state. The 120s deadline
   continues to govern genuinely silent children.
3. **Child attachability is already real** (children are normal
   sessions): verify `session.attach` + `menu.answer` against a child
   session work over UDS with Control (the TUI lane will ride this) —
   pin it with a live test if uncovered.
4. **Sequential spawn contract documented.** The codex responses-lite
   history contract (one tool call per round; a call's result must pair
   with its call) FORCES sequential spawn→report rounds — document in
   the delegation charter why the round parks instead of acking, so the
   next reader doesn't "fix" it into a broken parallel ack.

## Laws

Standing lane laws (tests never inline; mutation docs w/ RUNTIME
failures; CARGO_INCREMENTAL=0; fmt + clippy -D warnings; additive
protocol; ledger; no haider-tui; no Cargo.lock; no versions; leave
uncommitted; no git). Minimum tests:
- Child opens a permission menu → the parent journal carries the
  PermissionRequired chip state (mutation: drop the mirror → fails).
- A permission-parked child is NOT nudged/cancelled while parked; the
  clock resumes on unpark (mutation: stall clock ignores the park →
  the no-cancel law fails).
- Control attach + menu.answer on a child session resolves the child's
  menu and the child proceeds to Done; the parent collects (live UDS).

Use up to 2 research subagents and 1 verify subagent. Print a final
summary of files changed and tests added.
