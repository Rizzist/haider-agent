# G5 — responsive markdown table rendering: implementation notes

Branch `g5-table-rendering` (from main @ v0.0.76). Owner contract: GFM
tables render like Claude Code's — a bordered grid when the terminal is
wide enough, stacked labelled records below the breakpoint.

## The two reference layouts (owner screenshots, described)

**Wide mode — bordered grid.** A full box-drawing grid spanning the
content column: `┌─┬─┐` top border, one vertical `│` at every column
boundary with one space of padding on each side of every cell, a
`├─┼─┤` rule separating the header row from the body, `└─┴─┘` bottom
border. The header row's cell text is bold; body cells are ordinary
text with inline markdown (bold/italic/code) styled inside them. When
the columns' natural widths overflow the budget, cells word-wrap WITHIN
their columns (row height grows to the tallest cell) and per-column
alignment from the delimiter row (`:--`/`:-:`/`--:`) places every
wrapped line. No rules between body rows — only the header rule.

**Narrow mode — stacked records.** Below the breakpoint the grid
disappears entirely. Each BODY row becomes one block of `Label: value`
lines — one line per column, the header label and its colon bold, the
value in ordinary text — wrapping to the full content budget with no
hanging indent:

    Feature: Plate calculator
    What it does: Tap a weight → which plates per side
    Why it matters: Beloved quality-of-life touch
    Effort: Small
    ──────────────────────────────

A horizontal rule of `min(budget, 48)` `─` cells separates consecutive
records (between records only). The header row never emits a block of
its own; alignment hints are ignored; an empty cell keeps its labelled
line so every record has the same shape.

## Seams (the split the brief locks)

- `crates/haider-tui/src/md.rs` — parse stays WIDTH-AGNOSTIC:
  `render_markdown` tags each table source line with an `MdTableRow`
  (role Header/Delimiter/Body, per-cell span vectors, per-column
  alignments) riding a new `MdLine.table: Option<MdTableRow>` field.
  One `MdLine` per source line — LINE STABILITY holds through tables.
- Width applies at DRAW time only: `md::layout_table(rows, budget)` is a
  pure function returning display rows that already fit the budget;
  render.rs (`item_lines`, AgentMessage arm) groups consecutive
  table-tagged lines (the parser emits header/delimiter/body as one
  contiguous run) and pushes layout rows railed, never re-wrapped.
  Non-table lines keep the exact `wrap_spans` path they had.
- `MdKind::TableBorder` (new) styles all chrome; `Theme::md_style` maps
  it to `frame_style()` — theme slots, never raw colors. Header bold
  reuses `MdKind::Bold` via `nest(Bold, kind)` re-kinding.

## Measure + mode selection (decision 2)

Per table, per draw: natural width `N_c` = widest header/body cell;
floor `M_c` = max(longest unbreakable word, 3); chrome = `3*columns+1`
(a `│` per boundary + 1 space padding per cell side, full outer
border).

- `sum(N)+chrome <= budget` → natural grid (grid width = content, not
  stretched to the budget).
- else `sum(M)+chrome <= budget` → wrapped grid: floors first, the
  remaining budget distributed proportionally to each column's natural
  HEADROOM (`N_c - M_c`), integer leftovers one cell at a time
  left-to-right, no column past its natural width. (Equivalent in
  spirit to "proportional to N with floor M", stated so the floors are
  exact and the distribution deterministic.)
- else → stacked records (the breakpoint).

The choice is a pure function of the current budget — a resize flips
modes with no state. Pinned: the same source renders a grid at width
120 and stacked records at width 48 (`transcript_flips_grid_to_stacked_
with_terminal_width`).

## Choices pinned in goldens

- FULL outer border (`┌┐└┘` corners), header rule, no body-row rules —
  the Claude Code look; goldens in `lg1_natural_grid_golden_at_wide_
  budget` / `lg2_wrapped_grid_distributes_with_floors_and_alignment`.
- Natural grid hugs its content width; only the wrapped grid consumes
  the whole budget.
- Stacked rules go BETWEEN records only (none after the last).
- A header+delimiter table with no body rows yet (streaming) draws as a
  bordered header with no body section in grid mode and NOTHING in
  stacked mode (the header row never emits a block).
- The streaming cursor `▮` rides the LAST CELL of a streaming table row
  (`MdLine::push_cursor`), so the grid keeps the cursor visible.

## Parse details (decision 1)

- A table needs: a header line with ≥1 unescaped `|` that is NOT a
  heading/bullet/numbered line (the `block_mark` grammar, factored out
  of `block_line`), then a delimiter line whose cell count EQUALS the
  header's, every cell `:?-+:?`. Body rows continue while lines keep an
  unescaped pipe; a blank line, pipe-less line, or fence opener ends
  the table.
- `\|` is literal inside cells (the backslash is consumed); it is the
  only escape this renderer honours.
- Ragged body rows pad to the header's column count; excess cells drop.
- A header whose delimiter never arrives renders as an ordinary
  paragraph — the streaming prefix reclassifies cleanly on the frame
  where the delimiter lands (LST1).

## Plain/copy path (decision 6)

Table lines degrade to pipe-separated cells: `MdLine::plain()` returns
`| a | b |` with pad/truncate applied and `\|` unescaped; the delimiter
line stays verbatim. Lossless-enough for copy; the raw inter-cell
whitespace is normalized (single spaces around pipes). Plain mode
(`haider run` stdout, plain.rs) is untouched — raw markdown passes
through as before.

## Deviations from the brief's letter (spirit kept)

- "a new MdKind table variant carrying per-cell span vectors": `MdKind`
  is `Copy` and rides every wrapped character cell in `wrap_spans`; a
  data-carrying variant would break that contract crate-wide. The table
  facts live in `MdLine.table: Option<MdTableRow>` instead — same
  information, same width-agnostic seam, zero disturbance to the
  existing span machinery. The new `MdKind::TableBorder` variant covers
  the style vocabulary.
- Distribution is headroom-proportional above floors (see above) rather
  than raw-N proportional with post-hoc floor clamping; deterministic
  and floor-exact, pinned by LG2 goldens.

## Laws

`crates/haider-tui/tests/g5_table_tests.rs`: LP1 (5 tests + plain-path
pin), LG1 golden, LG2 golden + floor law + budget-fit sweep, LS1 golden
+ empty-cell law, LB1 three-budget breakpoint (mutation anchor), LST1
prefix sweep + frame-equality integration, cursor-in-cell, and the
width-flip integration. Every pre-existing md/render law untouched and
green (851 passing in haider-tui). Mutation evidence:
`G5-table-rendering-mutation-notes.md` (4 executed kills, including one
survival that forced a stronger LG2 fixture).
