# Patch brief W2a/C4a.3 — round-4: indivisible apply+ledger+outcome

Worktree /Users/rizzist/Documents/CODING/haider-agent-c4a, branch w2-c4a. ONE finding
(docs/briefs/C4a-review-3-NO_SHIP.md): cancellation after rename but during/before the ledger
append can drop the join handle so the failed outcome is never journaled → silent write.

Fix: make rename + ledger-append + outcome-decision ONE indivisible unit that cannot be
severed by outer-task cancellation:
- Move the whole post-verify critical section into a SINGLE spawn_blocking (or a
  cancellation-shielded block) that: renames temp→target, appends the ledger entry (bytes-hash),
  and RETURNS a value that fully determines the terminal outcome (Ok | LedgerFailed{written:true}).
  The join handle result maps deterministically to the journaled outcome. If the outer future is
  cancelled, the blocking work still runs to completion and its result is still consumed — do NOT
  drop the handle: await it in a cancellation-shielded finalizer, or structure so `finish`
  records the outcome from the handle result unconditionally.
- Invariant, restated in the header: once the rename lands, the outcome IS journaled (Ok or
  Failed-with-ledger-error) — no path drops the result. A ledger append failure after a
  successful write is `Failed` outcome carrying the ledger error, never silence.
- Test the exact intersection: cancel the outer task AND fail the ledger, racing the apply
  window → assert either (no rename, no ledger, clean cancel) OR (rename happened AND an outcome
  was journaled — Failed-with-ledger-error). NEVER rename-without-outcome.
Gate: cargo test -p haider-tools, workspace clippy -D warnings, fmt, xtask test-count --update.
Leave uncommitted.
