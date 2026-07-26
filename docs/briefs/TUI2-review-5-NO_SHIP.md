# TUI2 review round 5 — NO_SHIP (delta review of TUI2.6)

- Reviewer: gpt-5.6 (codex exec, detached), 2026-07-26
- Frozen SHA reviewed: 08f3702 (scope 68d6861..08f3702)
- Closure: P2-2 streaming-cursor CLOSED (literal ▮ intact, exact-fill railed, ≤3-width
  rail-only clean). P2-1 and P3-3 PARTIAL — residues below. dump_screens byte-identical
  to r4 at 90×8+/90×10/118×36; baseline 258→261 no deletions; tree byte-identical.
- Full log: ~/haider-run/tui2-review-r5.log

## New findings (to fix in TUI2.7)

P1: none.

P2
1. Blocking options invisible below 90×7 (render.rs:35/388/403/637): once transcript/gap/
   hint/body/title have yielded, the FIXED chrome (header 2 + header rule + input rule +
   status row) cannot be reclaimed — at 90×6 only "Allow once" renders, at 90×5 neither
   option. Clipped options get no hit region. Sim maps every option unconditionally
   (tui.js:3068). Fix: chrome yields progressively to a blocking menu.
2. Raw wheel bursts re-bank G16 debt and swallow reversal (app.rs:893, runtime.rs:224):
   100 queued wheel-ups then 1 wheel-down before any frame → scroll_back=297 → frame
   clamps to 6 — the down-notch never moves the view. The select loop does not guarantee
   a frame between queued inputs. The three r4-updated assertions lost burst coverage.

VERDICT: NO_SHIP — blocking controls disappear at 90×6/90×5; wheel bursts bank debt.
