# TUI2 review round 6 — NO_SHIP (delta review of TUI2.7)

- Reviewer: gpt-5.6 (codex exec, detached), 2026-07-26
- Frozen SHA reviewed: f2d5f2e (scope 08f3702..f2d5f2e)
- Closure: BOTH r5 findings CLOSED — chrome-yield/option-viewport verified at
  90×6/5/2/1 (windowing, ⋮, true-index hits, digit-answers-hidden, BackChip guard,
  status restoration; r5/r6 buffers byte-identical at 90×7/8/10 and 118×36);
  wheel reconcile-before-apply verified incl. the 100-up/1-down production repro.
- Full log: ~/haider-run/tui2-review-r6.log

## New findings (to fix in TUI2.8)

P1: none.

P2
1. 90×5 menu-CLOSE transition hides the composer + phantom TalkChip hit
   (render.rs:462/498/903): after MenuAnswered the status returns and the NON-menu
   ledger over-constrains (4 chrome + sacred transcript + forced 1-row composer > 4
   body rows) — the frame has header/rules/transcript/status but NO editable
   composer, while TalkChip is still emitted at (78,4) over the status row; at 90×1
   the hit is outside the frame. Pre-existing for ordinary tiny sessions; exposed by
   the close-path sweep. Fix: composer inherits the chrome-shed ladder (composer row
   is sacred in the non-menu ledger exactly as options are in the menu ledger), and
   hits are suppressed for zero-height/out-of-frame regions (general guard).

P3
2. Resize-before-redraw up-notch holds the last-known top (review4_fix_tests.rs:183)
   — ACCEPTED law consequence, documented in-test; live-geometry parity gap ledgered.
3. Stale doc: app.rs:188 field comment still says wheel does not clamp against
   scroll_max — contradicts the implementation. Fix the comment.

VERDICT: NO_SHIP — the 90×5 close path hides the sacred composer and leaves a
phantom click region.
