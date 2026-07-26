# TUI2 review round 7 — SHIP

- Reviewer: gpt-5.6 (codex exec, detached), 2026-07-26
- Frozen SHA reviewed: e7b8362 (scope f2d5f2e..e7b8362)
- Closure: round-6 P2 (menu-close composer starvation + phantom hit) CLOSED — symmetric
  ladder + hit-seam guard verified, editability tested at 90×5/90×1/90×2; hit filter
  confirmed to preserve legitimate visible options. Both P3s CLOSED (docs + ledger).
- Regression: menu paths pass at 90×7/6/5/2/1; boot/help at 90×1 no panic; shared dumps
  byte-identical incl. independent 118×36/90×10 comparisons; wheel/sticky green;
  baseline 266→270 with zero existing-test changes; tree byte-identical.

## Non-blocking P3s (carry to next TUI wave)
1. render.rs:450 — at 90×6 post-menu-close the explicit ladder retains the session
   subtitle and drops the transcript row, where the old renderer implicitly did the
   reverse; correct per the documented priority, but disproves the literal
   "every previously-fitting height byte-unchanged" claim.
2. render.rs:1139 — at 90×1 the help overlay does not clear the underlying row
   (text bleed-through: "help  esc closesssion…"). No panic, no active hits; polish debt.

VERDICT: SHIP — all P1/P2 gates clear; the two P3 tiny-layout polish findings are
non-blocking.

## Arc summary (r2..r7)
r2 NO_SHIP (1 P1 envelope race + 10 P2) → r3 NO_SHIP (5 P2) → r4 NO_SHIP (2 P2) →
r5 NO_SHIP (2 P2, extremes) → r6 NO_SHIP (1 P2, close-path) → r7 SHIP.
Fix rounds TUI2.2..TUI2.8 (31fd0ea, 46aff07, fc9793a, 08f3702, f2d5f2e, e7b8362).
Ordinary-size layouts byte-stable from r4 onward.
