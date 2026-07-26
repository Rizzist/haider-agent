# TUI2 review round 3 — NO_SHIP (delta review of TUI2.4)

- Reviewer: gpt-5.6 (codex exec, detached), 2026-07-26
- Frozen SHA reviewed: fc9793a (scope 46aff07..fc9793a)
- Closure of round-2 findings: P1-1, P2-2, P2-3, P2-5, P2-7, P2-11 CLOSED.
  P2-4/6/8/9/10 + P3-12 PARTIAL — residue re-filed as the new findings below.
- Reviewer re-ran all gates + both PTY probes incl. the input-driving ml probe;
  tree byte-identical after review. Full log: ~/haider-run/tui2-review-r3.log

## New findings (to fix in TUI2.5)

P1: none.

P2
1. Short-session input is NOT sacred (render.rs:396/725/629): at 90×10 a 4-5 line
   composer hides the active tail/cursor; the six-row permission menu renders only
   title/body — options invisible AND unclickable.
2. Scroll debt residue (app.rs:778/881, runtime.rs:276): fresh session inherits the
   previous transcript ceiling; enlarge-while-scrolled clamps against the old ceiling.
3. Sticky jump can reveal the wrong prompt (render.rs:511, app.rs:856): with prompts
   A then B, clicking sticky B jumps to B but the next render pins A over it. Sim
   clears + suppresses sticky until a real scroll (tui.js:2637).
4. Menu body not visually row-budgeted (render.rs:360/586): long/multi-line body
   clips while option hit rows assume one row per body string. Sim pre-wraps
   (tui.js:4946).
5. Agent pre-wrap lossy (render.rs:1109/1216): split_whitespace collapses internal
   runs/tabs/trailing whitespace; widths < 11 cells lose the rail on implicit
   continuation rows. Sim white-space: pre-wrap (tui.js:4508).

P3
6. Idle decay generation-scoped not session-scoped (runtime.rs:341): session A's
   decay can land in fresh session B within 30s (sim checks session id, tui.js:1555).
7. Fix tests bypass production wiring (review2_fix_tests.rs:95 calls consume_scripted
   directly; :373 inverts production resize ordering); missing: 4-line composer at
   90×10, narrow menus, multi-prompt sticky, exact whitespace preservation.

VERDICT: NO_SHIP — hidden active controls at 90×10 and persistent scroll debt are
interaction defects; no P1 remains.
