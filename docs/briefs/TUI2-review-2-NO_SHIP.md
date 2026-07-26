# TUI2 review round 2 — NO_SHIP

- Reviewer: gpt-5.6 (codex exec, detached), 2026-07-26
- Frozen SHA reviewed: 46aff07 (scope c101712..46aff07 — TUI2.2 composer/mouse + TUI2.3 audit gap closure)
- Reviewer re-ran gates itself (tests/clippy/fmt/dump_screens/PTY probes) and confirmed the
  tree byte-identical after review. Full log: ~/haider-run/tui2-review-r2.log

## Findings (to fix in TUI2.4)

P1
1. Post-interrupt envelope race: enqueue checks generation (runtime.rs:237) but consumption
   does not (runtime.rs:291); Esc can bump generation, then a buffered stale UserMessage
   re-sets turn_active — permanently-active UI with no script. Fix: generation-tag every
   envelope through the channel and check at consumption (sim token guard tui.js:1551-1567).

P2
2. Stale hit maps: previous frame's rects resolve by row INDEX against current palette items
   (app.rs:751) — backspace-then-click executes the wrong command; dismissed/overlay-covered
   palettes still clickable. Fix: value-carrying hits + palette-open guard (+ revision guard).
3. idle(i) never decays without typing; sim decays after 30s. Fix: runtime 30s timer →
   guarded decay event.
4. Composer advertises ⇧⏎ newline but Shift+Enter submits; no horizontal viewport so long
   input hides the cursor. Fix: real multi-line composer slice (see brief).
5. Palette arg-slot semantics diverge: sim shows args at exact `/theme` (no space), Enter/click
   on the row ENTERS the slot (acceptSuggestion) instead of executing; wrap must cover the
   full list (scroll window), not min(items, 8).
6. scroll_max starts at u16::MAX and is one frame stale; wheel-before-first-frame creates
   invisible debt. Fix: init 0 + clamp on wheel AND resize.
7. Paste thresholds: sim measures raw UTF-16 code units pre-normalization; ours counts scalars
   post-CRLF-normalization. Fix: encode_utf16().count() on the raw string.
8. Menu.body lines never rendered (height nor content) — permission cards can hide their
   authorization context. Fix: dim body lines between title and options (sim tui.js:3063-3067).
9. Sticky click yanks to live tail; sim jumpToSticky stays AT the producing prompt
   (tui.js:2637-2645). Chrome (underline/bar_bg) also diverges from sim StickyLine — match
   tui.js:4597-4623 exactly.
10. Agent body wrap counts chars not cells, never splits overlong words, collapses newlines/
    indentation (split_whitespace) — rail lost on implicit continuation rows; pre-wrap broken.
11. IDLE_I badge gold; sim falls through to dim outline (tui.js:5531-5547). (Round-2 direction
    error — sim truth wins.)

P3
12. Parity tests mirror the implementation (sticky-to-tail, post-space-only arg slots, bare-
    cycle pinned); missing coverage: interrupt ordering, stale hit maps, first-frame scroll,
    unicode paste, IDLE_I color, menu bodies, wide/pre-formatted agent text. Commit-message
    test counts were off (18 r2 tests not 15; 11 hit tests not 10); baselines correct.

## Parity spot-checks
MATCH: InputBar chrome, CmdMenu chrome/rects, todos styling, prefix ghost, honest compaction.
DIVERGES: InputBar newline/growth, /theme slot + wrap, badge tones (IDLE_I), agent rail
pre-wrap, sticky origin.

VERDICT: NO_SHIP — post-interrupt envelope race is ship-blocking; interaction and exact-parity
defects accompany it.
