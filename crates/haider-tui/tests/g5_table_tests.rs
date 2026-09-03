//! G5 — responsive markdown table rendering: grid ↔ stacked breakpoint.
//!
//! The laws, in the order the brief pins them:
//! * LP1 parse — GFM header+delimiter+body recognized, colon alignment
//!   parsed, escaped `\|` literal, inline markdown styled INSIDE cells,
//!   ragged rows pad/truncate to the header, a header whose delimiter
//!   never arrives renders as ordinary paragraphs.
//! * LG1 natural grid — goldens on the drawn lines at a wide budget:
//!   border glyphs, bold header, alignment honoured.
//! * LG2 wrapped grid — proportional widths with per-column floors
//!   (longest unbreakable word, minimum 3), multi-line rows, alignment
//!   still honoured.
//! * LS1 stacked — bold `Header:` labels, full-budget wrapping, rule
//!   separators between records, the header row never a block of its own.
//! * LB1 breakpoint — the EXACT same source at three budgets renders
//!   natural / wrapped / stacked (the mutation anchor).
//! * LST1 streaming — every prefix renders without panic and line-stable;
//!   the final frame equals the all-at-once render.
//!
//! Everything runs against the pure `md` seam or ratatui's TestBackend —
//! no real terminal.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::envelope::{EventEnvelope, PromptRender, RawEnvelope, RenderTargets};
use haider_protocol::ids::{DeviceId, EventId, ItemId, SessionId};
use haider_protocol::item::{ItemDelta, ItemEvent, TurnItem};
use haider_tui::app::{AppModel, RuntimeMode};
use haider_tui::md::{
    MdAlign, MdKind, MdLine, MdSpan, MdTableRole, MdTableRow, layout_table, render_markdown,
};
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Modifier;

mod common;
use common::launcher_model;

/// The law table: two columns, explicit left + right alignment, one cell
/// that must wrap ("Plate calculator"). Natural widths 16+6, chrome 7 →
/// natural total 29; floors 10+6, floor total 23 — so budget 80 is
/// natural, 26 is wrapped, 18 is stacked.
const TABLE: &str =
    "| Feature | Effort |\n| :------ | -----: |\n| Plate calculator | Small |\n| Sync | Large |";

/// The owner's reference table (the screenshot pair): four columns whose
/// floors exceed a 48-wide terminal's budget — grid at width 120,
/// stacked records at width 48.
const WIDE_TABLE: &str = "| Feature | What it does | Why it matters | Effort |\n| --- | --- | --- | --- |\n| Plate calculator | Tap a weight → which plates per side | Beloved quality-of-life touch | Small |\n| Rest timer | Auto-starts between sets | Keeps sessions honest | Medium |";

fn table_refs(lines: &[MdLine]) -> Vec<&MdTableRow> {
    lines
        .iter()
        .filter_map(|line| line.table.as_ref())
        .collect()
}

fn layout_rows(src: &str, budget: usize) -> Vec<Vec<MdSpan>> {
    let lines = render_markdown(src);
    layout_table(&table_refs(&lines), budget)
}

fn row_texts(rows: &[Vec<MdSpan>]) -> Vec<String> {
    rows.iter()
        .map(|row| row.iter().map(|span| span.text.as_str()).collect())
        .collect()
}

fn cell_plain(cell: &[MdSpan]) -> String {
    cell.iter().map(|span| span.text.as_str()).collect()
}

fn display_width(row: &[MdSpan]) -> usize {
    use unicode_width::UnicodeWidthChar;
    row.iter()
        .flat_map(|span| span.text.chars())
        .map(|ch| UnicodeWidthChar::width(ch).unwrap_or(0))
        .sum()
}

// ---- LP1: parse ----

/// MUTATION CHECK (G5): make `parse_delimiter` accept a mismatched cell
/// count. Expected runtime failure: the missing-delimiter paragraph
/// assertions below find table tags.
#[test]
fn lp1_table_recognized_with_roles_and_alignment() {
    let lines = render_markdown(TABLE);
    assert_eq!(
        lines.len(),
        4,
        "one MdLine per source line — LINE STABILITY"
    );
    let rows = table_refs(&lines);
    assert_eq!(rows.len(), 4, "every table line is tagged");
    assert_eq!(
        rows.iter().map(|r| r.role).collect::<Vec<_>>(),
        vec![
            MdTableRole::Header,
            MdTableRole::Delimiter,
            MdTableRole::Body,
            MdTableRole::Body
        ]
    );
    for row in &rows {
        assert_eq!(row.aligns, vec![MdAlign::Left, MdAlign::Right]);
    }
    assert_eq!(cell_plain(&rows[0].cells[0]), "Feature");
    assert_eq!(cell_plain(&rows[0].cells[1]), "Effort");
    assert_eq!(cell_plain(&rows[2].cells[0]), "Plate calculator");
    assert_eq!(cell_plain(&rows[3].cells[1]), "Large");

    // The full colon vocabulary on one delimiter row.
    let aligned = render_markdown("| a | b | c | d |\n| :-- | :-: | --: | --- |");
    let rows = table_refs(&aligned);
    assert_eq!(
        rows[0].aligns,
        vec![
            MdAlign::Left,
            MdAlign::Center,
            MdAlign::Right,
            MdAlign::Left
        ]
    );
}

/// MUTATION CHECK (G5): drop the `\|` arm in `split_cells` (treat the
/// escaped pipe as a delimiter). Expected runtime failure: the column
/// count changes and the literal-pipe cell text assertions fail.
#[test]
fn lp1_escaped_pipe_stays_literal_inside_its_cell() {
    let lines = render_markdown("| a \\| b | c |\n| --- | --- |\n| d \\| e | f |");
    let rows = table_refs(&lines);
    assert_eq!(rows.len(), 3, "the escaped pipe never splits a cell");
    assert_eq!(rows[0].aligns.len(), 2, "two columns, not three");
    assert_eq!(cell_plain(&rows[0].cells[0]), "a | b");
    assert_eq!(cell_plain(&rows[2].cells[0]), "d | e");
    assert_eq!(cell_plain(&rows[2].cells[1]), "f");
}

/// Inline markdown renders INSIDE cells through the ordinary span
/// machinery — bold, code, italic keep their kinds.
#[test]
fn lp1_inline_styling_renders_inside_cells() {
    let lines = render_markdown("| **bold** | `code` |\n| --- | --- |\n| *it* | plain |");
    let rows = table_refs(&lines);
    assert_eq!(rows[0].cells[0][0].kind, MdKind::Bold);
    assert_eq!(rows[0].cells[0][0].text, "bold");
    assert_eq!(rows[0].cells[1][0].kind, MdKind::Code);
    assert_eq!(rows[0].cells[1][0].text, "code");
    assert_eq!(rows[2].cells[0][0].kind, MdKind::Italic);
    assert_eq!(rows[2].cells[1][0].kind, MdKind::Text);
}

/// Ragged body rows pad to the header's column count; excess cells drop
/// (GFM).
#[test]
fn lp1_ragged_rows_pad_and_truncate_to_the_header() {
    let lines = render_markdown("| a | b | c |\n| - | - | - |\n| x |\n| p | q | r | s |");
    let rows = table_refs(&lines);
    assert_eq!(rows[2].cells.len(), 3, "short row pads to three cells");
    assert_eq!(cell_plain(&rows[2].cells[0]), "x");
    assert_eq!(cell_plain(&rows[2].cells[1]), "");
    assert_eq!(cell_plain(&rows[2].cells[2]), "");
    assert_eq!(rows[3].cells.len(), 3, "long row truncates to three cells");
    assert_eq!(cell_plain(&rows[3].cells[2]), "r");
}

/// A header whose delimiter never arrives is NOT a table — both lines
/// render as ordinary paragraphs with the pipes literal. Heading- and
/// bullet-marked lines never open a table either.
#[test]
fn lp1_missing_delimiter_and_marked_lines_stay_paragraphs() {
    let lines = render_markdown("| a | b |\nplain text after");
    assert!(lines.iter().all(|line| line.table.is_none()));
    assert_eq!(lines[0].plain(), "| a | b |");
    assert!(lines[0].spans.iter().all(|s| s.kind == MdKind::Text));

    let alone = render_markdown("| a | b |");
    assert!(alone[0].table.is_none(), "no delimiter yet — a paragraph");

    let heading = render_markdown("# h | x\n| --- |\nafter");
    assert!(heading.iter().all(|line| line.table.is_none()));
    assert_eq!(heading[0].spans[0].kind, MdKind::HeadingMark);

    let bullet = render_markdown("- item | pipe\n| --- | --- |");
    assert!(bullet.iter().all(|line| line.table.is_none()));
    assert_eq!(bullet[0].spans[0].kind, MdKind::ListMark);
}

/// Decision 6: the plain/copy path degrades a table to pipe-separated
/// cells (pad/truncate applied, `\|` unescaped), the delimiter line
/// verbatim — lossless-enough for copy.
#[test]
fn table_plain_path_is_pipe_separated_cells() {
    let lines = render_markdown(TABLE);
    assert_eq!(lines[0].plain(), "| Feature | Effort |");
    assert_eq!(lines[1].plain(), "| :------ | -----: |");
    assert_eq!(lines[2].plain(), "| Plate calculator | Small |");
    assert_eq!(lines[3].plain(), "| Sync | Large |");
}

// ---- LG1: natural grid ----

/// MUTATION CHECK (G5): break the alignment arm (treat Right as Left in
/// `grid_row`). Expected runtime failure: ` Small` loses its leading pad
/// and the golden literals below mismatch.
#[test]
fn lg1_natural_grid_golden_at_wide_budget() {
    let rows = layout_rows(TABLE, 80);
    assert_eq!(
        row_texts(&rows),
        vec![
            "┌──────────────────┬────────┐".to_owned(),
            "│ Feature          │ Effort │".to_owned(),
            "├──────────────────┼────────┤".to_owned(),
            "│ Plate calculator │  Small │".to_owned(),
            "│ Sync             │  Large │".to_owned(),
            "└──────────────────┴────────┘".to_owned(),
        ]
    );
    // Chrome is TableBorder ink; header cells are BOLD; body stays Text.
    assert_eq!(rows[0].len(), 1);
    assert_eq!(rows[0][0].kind, MdKind::TableBorder);
    let header_feature = rows[1]
        .iter()
        .find(|span| span.text == "Feature")
        .expect("header cell span");
    assert_eq!(header_feature.kind, MdKind::Bold);
    let body_cell = rows[3]
        .iter()
        .find(|span| span.text == "Plate calculator")
        .expect("body cell span");
    assert_eq!(body_cell.kind, MdKind::Text);
}

// ---- LG2: wrapped grid ----

/// MUTATION CHECK (G5): distribute without floors (proportional shares
/// from zero). Expected runtime failure: the border widths change and
/// the golden literals below mismatch.
#[test]
fn lg2_wrapped_grid_distributes_with_floors_and_alignment() {
    // Budget 26: floors [10, 6] fit (23 ≤ 26), naturals [16, 6] do not
    // (29 > 26). Headroom-proportional distribution gives column 0 the
    // three spare cells → widths [13, 6]; "Plate calculator" wraps to
    // two rows; the right-aligned column keeps its alignment on every
    // row, including the blank continuation cell.
    let rows = layout_rows(TABLE, 26);
    assert_eq!(
        row_texts(&rows),
        vec![
            "┌───────────────┬────────┐".to_owned(),
            "│ Feature       │ Effort │".to_owned(),
            "├───────────────┼────────┤".to_owned(),
            "│ Plate         │  Small │".to_owned(),
            "│ calculator    │        │".to_owned(),
            "│ Sync          │  Large │".to_owned(),
            "└───────────────┴────────┘".to_owned(),
        ]
    );
}

/// MUTATION CHECK (G5): pin the floor to the constant 3 (drop the
/// longest-word measure). Expected runtime failure: the prose column's
/// larger natural headroom out-competes the word column in the
/// distribution, the word column drops below 21 cells, the unbreakable
/// word hard-splits, and the intact-word assertion finds no row carrying
/// it. (A first fixture without a competing column let this mutation
/// SURVIVE — the word column won enough width from headroom alone; the
/// floor only binds under competition, so the law pits the two columns
/// against each other.)
#[test]
fn lg2_column_floor_holds_the_longest_word_and_the_minimum() {
    // Competition fixture: the word column's floor IS its natural width
    // (21), the prose column's natural (44) dwarfs it. Budget 40:
    // naturals overflow (72), floors [21, 5] fit (33). All headroom
    // belongs to the prose column, so the word column sits exactly on
    // its longest-word floor — 23 border cells — and the word survives
    // intact.
    let src = "| A | B |\n| - | - |\n| unbreakable_word_here | lots of small words that go on and on and on |";
    let rows = layout_rows(src, 40);
    let texts = row_texts(&rows);
    assert!(
        texts[0].starts_with("┌───────────────────────┬"),
        "the word column holds its longest-word floor of 21: {:?}",
        texts[0]
    );
    assert!(
        texts
            .iter()
            .any(|row| row.contains("unbreakable_word_here")),
        "the longest word never hard-splits: {texts:?}"
    );

    // Minimum fixture: a column of single-char cells still gets the
    // floor of 3 (5 border cells), never its natural width of 1.
    let tiny = "| K | Note |\n| - | ---- |\n| a | unbreakable_word_here and more prose text |";
    let rows = layout_rows(tiny, 34);
    let texts = row_texts(&rows);
    assert!(
        texts[0].starts_with("┌─────┬"),
        "column K sits on the minimum floor of 3 (5 border cells): {:?}",
        texts[0]
    );
    assert!(
        texts
            .iter()
            .any(|row| row.contains("unbreakable_word_here")),
        "the longest word never hard-splits in the tiny fixture: {texts:?}"
    );
}

/// Every layout row fits the budget at EVERY budget — grid chrome,
/// wrapped cells, stacked records, and rules alike.
#[test]
fn layout_rows_always_fit_the_budget() {
    for budget in 1..=60usize {
        for row in layout_rows(TABLE, budget) {
            assert!(
                display_width(&row) <= budget,
                "row {:?} overflows budget {budget}",
                cell_plain(&row)
            );
        }
    }
}

// ---- LS1: stacked records ----

/// MUTATION CHECK (G5): emit the stacked label with Text kind (skip the
/// embolden). Expected runtime failure: the label-kind assertions below
/// find Text where Bold is required.
#[test]
fn ls1_stacked_records_bold_labels_rules_no_header_block() {
    let rows = layout_rows(TABLE, 18);
    assert_eq!(
        row_texts(&rows),
        vec![
            "Feature: Plate ".to_owned(),
            "calculator".to_owned(),
            "Effort: Small".to_owned(),
            "──────────────────".to_owned(),
            "Feature: Sync".to_owned(),
            "Effort: Large".to_owned(),
        ]
    );
    // The label and its colon are BOLD (compressed into one span by the
    // wrap); the value stays Text; the rule between records is
    // TableBorder ink sized min(budget, 48) = 18.
    assert_eq!(rows[0][0].text, "Feature:");
    assert_eq!(rows[0][0].kind, MdKind::Bold);
    let value = rows[0].last().expect("value span");
    assert_eq!(value.kind, MdKind::Text);
    assert_eq!(rows[3].len(), 1);
    assert_eq!(rows[3][0].kind, MdKind::TableBorder);
    // The header row never emits a block of its own: no record reads
    // `Feature: Feature`, and exactly ONE rule separates the two body
    // records.
    let texts = row_texts(&rows);
    assert!(!texts.iter().any(|row| row == "Feature: Feature"));
    assert_eq!(
        texts.iter().filter(|row| row.starts_with('─')).count(),
        1,
        "rules sit BETWEEN records only"
    );
}

/// An empty cell keeps its labelled line — every record has the same
/// predictable shape.
#[test]
fn ls1_empty_cells_keep_their_labelled_line() {
    // Budget 8 sits below even this tiny table's natural total (9), so
    // the records stack; the empty cell still gets its labelled line.
    let rows = layout_rows("| A | B |\n| - | - |\n| x |  |", 8);
    assert_eq!(row_texts(&rows), vec!["A: x".to_owned(), "B: ".to_owned()]);
}

// ---- LB1: the breakpoint (mutation anchor) ----

/// MUTATION CHECK (G5, the anchor): widen the floor-fit test by 8 cells
/// (`floor_total <= budget + 8` in `layout_table`). Expected runtime
/// failure: budget 18 picks the wrapped grid and the stacked assertions
/// below find border glyphs.
#[test]
fn lb1_one_source_three_budgets_natural_wrapped_stacked() {
    // Budget 80 → NATURAL: six rows, no cell wraps, full border.
    let natural = row_texts(&layout_rows(TABLE, 80));
    assert_eq!(natural.len(), 6);
    assert!(natural[0].starts_with('┌'));
    assert!(natural.iter().any(|row| row.contains("Plate calculator")));

    // Budget 26 → WRAPPED: the same source grows a continuation row.
    let wrapped = row_texts(&layout_rows(TABLE, 26));
    assert_eq!(wrapped.len(), 7);
    assert!(wrapped[0].starts_with('┌'));
    assert!(wrapped.iter().any(|row| row.contains("calculator")));
    assert!(
        !wrapped.iter().any(|row| row.contains("Plate calculator")),
        "the wrapped grid split the cell"
    );

    // Budget 18 → STACKED: no grid glyphs at all, labelled records.
    let stacked = row_texts(&layout_rows(TABLE, 18));
    assert!(stacked.iter().all(|row| !row.contains('│')));
    assert!(stacked.iter().all(|row| !row.contains('┌')));
    assert!(stacked[0].starts_with("Feature:"));
}

// ---- LST1: streaming ----

/// Every char-boundary prefix of the table renders without panic, stays
/// line-stable, and lays out at every interesting budget; the final
/// prefix equals the all-at-once render. Before the delimiter lands the
/// header is an ordinary paragraph — the reclassification is clean.
#[test]
fn lst1_streaming_prefixes_render_and_settle() {
    let mut prefix = String::new();
    for ch in TABLE.chars() {
        prefix.push(ch);
        let lines = render_markdown(&prefix);
        assert_eq!(
            lines.len(),
            prefix.split('\n').count(),
            "prefix {prefix:?} broke line stability"
        );
        for budget in [1usize, 18, 26, 80] {
            let _ = layout_table(&table_refs(&lines), budget);
        }
    }
    assert_eq!(render_markdown(&prefix), render_markdown(TABLE));

    // The header alone is a paragraph; header+delimiter is already a
    // table (with no body yet).
    let header_only = render_markdown("| Feature | Effort |");
    assert!(header_only[0].table.is_none());
    let with_delimiter = render_markdown("| Feature | Effort |\n| :------ | -----: |");
    assert!(with_delimiter[0].table.is_some(), "delimiter reclassifies");
}

/// The streaming cursor rides the LAST CELL of a streaming table row, so
/// the grid keeps it visible.
#[test]
fn streaming_cursor_rides_the_last_table_cell() {
    let mut lines = render_markdown(TABLE);
    lines.last_mut().expect("tail").push_cursor();
    let rows = row_texts(&layout_table(&table_refs(&lines), 80));
    assert!(
        rows.iter().any(|row| row.contains("Large▮")),
        "the cursor renders inside the grid: {rows:?}"
    );
}

// ---- render integration: the transcript wears the table ----

fn sid() -> SessionId {
    SessionId::new("g5-table-session")
}

fn raw(seq: u64, payload: &EventPayload) -> RawEnvelope {
    EventEnvelope {
        schema_version: 1,
        event_id: EventId::new(format!("evt-g5-{seq}")),
        seq,
        session_id: sid(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("g5-device"),
        authority_epoch: 1,
        worker_generation: 1,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::to_value(payload).expect("payload serializes"),
    }
}

fn live_session() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.sessions.clear();
    model.upsert_live_session(&sid());
    model.open_session(&sid());
    model.requests.clear();
    model
}

fn feed_message(model: &mut AppModel, chunks: &[&str], complete: bool) {
    let item = ItemId::new("item-g5");
    model.route_raw(&raw(
        10,
        &EventPayload::Item(ItemEvent::Started {
            item_id: item.clone(),
            item: TurnItem::AgentMessage {
                text: String::new().into(),
            },
        }),
    ));
    let mut seq = 11u64;
    for chunk in chunks {
        model.route_raw(&raw(
            seq,
            &EventPayload::Item(ItemEvent::Delta {
                item_id: item.clone(),
                delta: ItemDelta::Text {
                    text: (*chunk).to_owned(),
                },
            }),
        ));
        seq += 1;
    }
    if complete {
        model.route_raw(&raw(
            seq,
            &EventPayload::Item(ItemEvent::Completed {
                item_id: item,
                item: TurnItem::AgentMessage {
                    text: chunks.concat().into(),
                },
            }),
        ));
    }
}

fn draw_cells(model: &AppModel, width: u16, height: u16) -> Vec<Vec<(String, Modifier)>> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            render(model, frame);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| {
                    let cell = &buffer[(x, y)];
                    (cell.symbol().to_owned(), cell.style().add_modifier)
                })
                .collect()
        })
        .collect()
}

fn draw_rows(model: &AppModel, width: u16, height: u16) -> Vec<String> {
    draw_cells(model, width, height)
        .iter()
        .map(|row| row.iter().map(|(sym, _)| sym.as_str()).collect())
        .collect()
}

/// Decision 5's pin: the SAME source renders a bordered grid at width
/// 120 and stacked records at width 48 — the flip is a pure function of
/// the draw budget, no state. Header cells carry BOLD in the grid;
/// stacked labels carry BOLD below the breakpoint.
///
/// MUTATION CHECK (G5): route the table lines through the ordinary
/// wrap_spans path in render.rs. Expected runtime failure: no border
/// glyph reaches the wide frame and the raw pipes reappear.
#[test]
fn transcript_flips_grid_to_stacked_with_terminal_width() {
    let mut model = live_session();
    feed_message(&mut model, &[WIDE_TABLE], true);

    let wide = draw_cells(&model, 120, 30);
    let wide_rows: Vec<String> = wide
        .iter()
        .map(|row| row.iter().map(|(sym, _)| sym.as_str()).collect())
        .collect();
    assert!(
        wide_rows.iter().any(|row| row.contains('┌')),
        "width 120 renders the bordered grid"
    );
    let header_row = wide_rows
        .iter()
        .position(|row| row.contains("│ Feature"))
        .expect("grid header row");
    let col = wide_rows[header_row]
        .chars()
        .collect::<Vec<_>>()
        .iter()
        .position(|&c| c == 'F')
        .expect("header cell col");
    assert!(
        wide[header_row][col].1.contains(Modifier::BOLD),
        "grid header cells render bold"
    );
    assert!(
        !wide_rows.iter().any(|row| row.contains("Feature:")),
        "no stacked labels in the wide frame"
    );

    let narrow = draw_cells(&model, 48, 40);
    let narrow_rows: Vec<String> = narrow
        .iter()
        .map(|row| row.iter().map(|(sym, _)| sym.as_str()).collect())
        .collect();
    assert!(
        narrow_rows.iter().all(|row| !row.contains('┌')),
        "width 48 drops the grid entirely"
    );
    let label_row = narrow_rows
        .iter()
        .position(|row| row.contains("What it does:"))
        .expect("stacked label row");
    let label_col = narrow_rows[label_row]
        .chars()
        .collect::<Vec<_>>()
        .iter()
        .position(|&c| c == 'W')
        .expect("label col");
    assert!(
        narrow[label_row][label_col].1.contains(Modifier::BOLD),
        "stacked labels render bold"
    );
    assert!(
        narrow_rows.iter().any(|row| row.contains("───")),
        "a rule separates the stacked records"
    );
}

/// LST1 at the frame level: the table fed line-by-line draws the same
/// final frame as the table fed in one delta, and a mid-table partial
/// frame renders without panic.
#[test]
fn lst1_final_frame_equals_the_all_at_once_render() {
    let mut all_at_once = live_session();
    feed_message(&mut all_at_once, &[WIDE_TABLE], true);

    let mut piecewise = live_session();
    let lines: Vec<String> = WIDE_TABLE
        .split_inclusive('\n')
        .map(str::to_owned)
        .collect();
    let chunks: Vec<&str> = lines.iter().map(String::as_str).collect();
    feed_message(&mut piecewise, &chunks, true);

    assert_eq!(
        draw_cells(&all_at_once, 120, 30),
        draw_cells(&piecewise, 120, 30),
        "the final frame is delta-schedule independent"
    );

    // A mid-table prefix (header + delimiter, body still coming) draws
    // cleanly at both extremes.
    let mut partial = live_session();
    feed_message(
        &mut partial,
        &["| Feature | What it does | Why it matters | Effort |\n| --- | --- | --- | --- |\n"],
        false,
    );
    let rows = draw_rows(&partial, 120, 30);
    assert!(
        rows.iter().any(|row| row.contains('┌')),
        "partial grid draws"
    );
    let _ = draw_rows(&partial, 48, 40);
}

/// Review pin (coordinator, G5 review of record): DOUBLE-WIDTH glyphs
/// (CJK) keep the grid aligned — cell padding and border placement are
/// computed from DISPLAY width, not char count. Every drawn line of the
/// table block must occupy the same number of terminal cells.
/// MUTATION CHECK: measure cells with chars().count() instead of
/// unicode display width. Expected RUNTIME failure: the CJK row's line
/// width diverges from the border rows below.
#[test]
fn double_width_glyphs_keep_the_grid_aligned() {
    use unicode_width::UnicodeWidthChar;
    let source = "| Name | Note |\n| --- | --- |\n| 日本語データ | ok |\n| ascii | ok |\n";
    let rows = layout_rows(source, 80);
    let widths: Vec<usize> = row_texts(&rows)
        .iter()
        .map(|line| {
            line.chars()
                .map(|ch| UnicodeWidthChar::width(ch).unwrap_or(0))
                .sum()
        })
        .collect();
    assert!(
        widths.windows(2).all(|pair| pair[0] == pair[1]),
        "every table line must span the same display width, got {widths:?}"
    );
    let texts = row_texts(&rows);
    let cjk_line = texts
        .iter()
        .find(|line| line.contains("日本語データ"))
        .expect("the CJK row renders");
    assert!(
        cjk_line.starts_with('│') && cjk_line.ends_with('│'),
        "borders survive on the CJK row: {cjk_line}"
    );
}
