# TUI2 review round 4 — NO_SHIP (delta review of TUI2.5)

- Reviewer: gpt-5.6 (codex exec, detached), 2026-07-26
- Frozen SHA reviewed: 68d6861 (scope fc9793a..68d6861)
- Closure of round-3 findings: P2-2, P2-3, P2-4, P3-6, P3-7 CLOSED (DemoDriver extraction
  verified behavior-equivalent). P2-1 and P2-5 PARTIAL — residues below.
- Reviewer re-ran all gates + all four PTY probes; ordinary layout + launcher mirror intact;
  tree byte-identical. Full log: ~/haider-run/tui2-review-r4.log

## New findings (to fix in TUI2.6)

P1: none.

P2
1. Blocking menu still loses an option at 90×7 (render.rs:385/427): the height ledger
   reserves one sacred transcript row while the six-row menu is over-constrained —
   ratatui starves the options region; "Deny" invisible AND unclickable. At 90×8 fine.
   Sim's flex transcript may collapse entirely; every option always mapped
   (tui.js:4444, 3068). Fix: menu options outrank the transcript row — the sacred
   transcript row yields when a blocking menu cannot otherwise fit its options.
2. Streaming cursor breaks the rail invariant (render.rs:1354/1367): body wraps to the
   full content budget, then ▮ is appended — when the last streaming row exactly fills
   its budget the cursor overflows into a rail-less implicit continuation row. Fix:
   account for the cursor cell inside the wrap (append before wrapping, or reserve a
   cell on the final streaming row).

P3
3. One wheel notch can be lost between resize and redraw (runtime.rs:314, app.rs:881):
   wheel clamps against the stale scroll_max, then render re-clamps. Since render is
   now the frame authority, wheel-time clamping against scroll_max is unnecessary —
   record intent saturating, let the frame reconcile (sim reads live DOM geometry).

VERDICT: NO_SHIP — a hidden blocking control (90×7) and rail-less streaming wraps remain.
