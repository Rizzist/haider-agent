# G5 — responsive markdown table rendering (grid ↔ stacked breakpoint)

Owner contract: GFM tables must render like Claude Code's — a bordered
grid when the terminal is wide enough, and BELOW THE BREAKPOINT a stacked
record layout ("Feature: …" / "What it does: …" lines with bold header
labels and rule separators) — owner: "this is the correct approach. have
this in the haider harness as well". Branch: `g5-table-rendering` (from
main @ v0.0.76). Reference screenshots described in
docs/briefs/G5-table-rendering-notes.md when you write it.

## Current seams

- `crates/haider-tui/src/md.rs` (418 lines): `render_markdown(text) ->
  Vec<MdLine>` is WIDTH-AGNOSTIC (MdKind + styled MdSpan runs);
  `wrap_spans(spans, budget)` applies width at draw time. NO table
  support today. Find the draw site(s) that consume MdLine +
  wrap_spans (render.rs assistant cells; check plain()/copy paths) and
  keep the same width-agnostic-parse / width-aware-draw split.

## Locked design decisions

1. PARSE (GFM pipe tables): header row + delimiter row
   (`| --- | :-: | --: |`, colon alignment) + body rows. Escaped `\|`
   is literal; inline markdown (bold/italic/code) renders INSIDE cells
   via the existing span machinery; ragged body rows pad to the header's
   column count, excess cells drop (GFM). A header row whose delimiter
   never arrives is NOT a table — it renders as ordinary paragraph
   lines. Represent rows as a new MdKind table variant carrying
   per-cell span vectors; consecutive table lines group into ONE table
   at draw time.
2. MEASURE (draw time, per table, per available budget):
   - natural col width N_c = max display width across header+cells;
   - min col width M_c = max(longest unbreakable word, 3);
   - chrome = vertical border per column boundary + 1-space padding
     each side of each cell.
   - sum(N)+chrome <= budget → natural grid.
   - else sum(M)+chrome <= budget → WRAPPED GRID: distribute budget
     proportionally to N_c with floor M_c; wrap cell spans per column
     (reuse wrap_spans); row height = tallest cell; alignment honored.
   - else → STACKED (the breakpoint).
3. GRID CHROME: follow the repo's existing border idiom (box-drawing
   glyphs used elsewhere in render.rs; header row separated from body by
   a horizontal rule; outer border optional if the existing cell idiom
   prefers open sides — match the TUI's look, pin whatever you choose in
   goldens). Header cells render bold.
4. STACKED LAYOUT (per BODY row, header row never emits its own block):
   for each column, one logical line `Header: value` — the header label
   bold + literal ": ", then the cell's spans; wraps to the full budget
   with no hanging indent. Between rows: a rule of `min(budget, 48)`
   horizontal-line glyphs. Alignment hints ignored. Empty cells render
   the label with an empty value (no line skipped — predictable shape).
5. STREAMING + RESIZE: cells re-render as text accumulates — partial
   tables must never panic and reclassify cleanly once the delimiter
   row lands; the grid↔stacked choice is made per draw from the CURRENT
   budget, so a resize flips modes with no state (pin: same source
   renders grid at width 120 and stacked at width 48).
6. `MdLine::plain()` / copy path: tables degrade to their raw-ish text
   (tab- or pipe-separated cells) so copy stays lossless-enough; note
   the choice. Plain mode (`haider run` stdout) is OUT OF SCOPE — raw
   markdown passes through as today.

## Mandatory laws (md_tests + tui render tests)

- LP1 parse: table recognized (header+delimiter+body), alignment
  parsed, escaped pipe literal, inline bold/code inside cells styled,
  ragged rows padded/truncated, missing-delimiter renders as paragraphs.
- LG1 natural grid at wide budget: goldens on the drawn lines (border
  glyphs, bold header, alignment).
- LG2 wrapped grid at medium budget: proportional widths with floors,
  multi-line row, alignment still honored.
- LS1 stacked at narrow budget: bold `Header:` labels, full-budget
  wrapping, rule separators, header row not emitted as a block.
- LB1 breakpoint: EXACT same source at three budgets → natural / wrapped
  / stacked (three assertions in one law; this is the mutation anchor).
- LST1 streaming: feeding the table line-by-line never panics and the
  final frame equals the all-at-once render.
- Regression: every pre-existing md/render law untouched and green.

## Discipline

Standard lane rules: CARGO_INCREMENTAL=0; cargo test -p haider-tui per
commit; `cargo fmt --all -- --check` clean at every commit; named-path
adds only; ledger `cargo run -p xtask -- test-count --update` before the
final commit, truthful old→new; write G5-table-rendering-notes.md +
G5-table-rendering-mutation-notes.md with ≥4 EXECUTED kills
(commit-before-mutation, single-anchor, "running 1 test" observed,
recorded failure, revert, green) covering at least: the breakpoint
comparison, the M_c floor, escaped-pipe handling, and the stacked bold
labels. No version bumps, no tags, no MCP, no renames. Tests must not
depend on a real terminal (the existing render-test harness pattern).
