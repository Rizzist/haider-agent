//! TUI6 — composer soft-wrap (owner wave off the v0.0.12 keystone).
//!
//! Items 1-5: the composer starts at ONE visual row and grows by WRAPPING
//! at grapheme boundaries — no horizontal windowing, no `…`, ever; the
//! autogrow cap and vertical window count VISUAL rows with the caret
//! always visible; ↑/↓ (and ⇧-extension) walk visual rows while Home/End
//! stay logical; clicks map through the wrapped row windows; wrap points
//! derive from the CURRENT width each frame (resize reflows, the model
//! stores no wrap state).
//!
//! Item 6 (band anatomy) and item 7 (header mark) live in this file too —
//! one owner wave, one suite.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_tui::app::{AppEvent, AppModel, ChipModel, ChipQuestion, Hit, RuntimeMode, Screen};
use haider_tui::composer::{Composer, wrap_rows};
use haider_tui::mock::demo_script;
use haider_tui::render::render;
use haider_tui::runtime::dispatch_input;
use haider_tui::script::{ChipDisplayState, ChipSeed};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use ratatui::style::Color;

mod common;
use common::{key, launcher_model};

fn key_mod(code: KeyCode, modifiers: KeyModifiers) -> AppEvent {
    AppEvent::Key(KeyEvent::new(code, modifiers))
}

fn session_model() -> AppModel {
    let mut model = AppModel::new();
    for payload in demo_script() {
        model.handle(AppEvent::Envelope(Box::new(payload)));
    }
    model
}

fn draw(
    model: &AppModel,
    width: u16,
    height: u16,
) -> (Vec<String>, Vec<(Rect, Hit)>, Terminal<TestBackend>) {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut hits = Vec::new();
    terminal
        .draw(|frame| hits = render(model, frame))
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let rows = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect();
    (rows, hits, terminal)
}

fn row_of(rows: &[String], needle: &str) -> u16 {
    u16::try_from(
        rows.iter()
            .position(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("row containing {needle:?} not rendered")),
    )
    .expect("row fits u16")
}

fn mouse(kind: MouseEventKind, column: u16, row: u16) -> Event {
    Event::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    })
}

/// The rendered composer text rows, in order (each carries its wrap
/// segment's byte range through the value-carrying hit).
fn composer_windows(hits: &[(Rect, Hit)]) -> Vec<(Rect, usize, String)> {
    let mut windows: Vec<(Rect, usize, String)> = hits
        .iter()
        .filter_map(|(rect, hit)| match hit {
            Hit::ComposerText { start, content, .. } => Some((*rect, *start, content.clone())),
            _ => None,
        })
        .collect();
    windows.sort_by_key(|(rect, _, _)| rect.y);
    windows
}

// ---- Item 1: one row initially, growth by wrapping, never an ellipsis ----

#[test]
fn composer_starts_at_one_row_and_grows_by_wrapping() {
    let mut model = session_model();
    let (_, hits, _) = draw(&model, 90, 34);
    assert_eq!(
        composer_windows(&hits).len(),
        1,
        "the empty composer is ONE visual row"
    );
    // A short draft stays one row.
    for c in "short draft".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    let (_, hits, _) = draw(&model, 90, 34);
    assert_eq!(composer_windows(&hits).len(), 1);
    // A draft past the row budget WRAPS — the owner's `❯ …jjjj…` report:
    // the row count grows, no horizontal extension, no `…` anywhere in
    // the band.
    model.composer.clear();
    for _ in 0..200 {
        model.handle(key(KeyCode::Char('j')));
    }
    let (rows, hits, _) = draw(&model, 90, 34);
    let windows = composer_windows(&hits);
    assert!(
        windows.len() >= 2,
        "200 cells at 90 cols wrap into visual rows, got {}",
        windows.len()
    );
    // The wrap is a partition of the draft: contiguous byte ranges, in
    // order, covering all 200 bytes.
    let mut expected_start = 0;
    for (_, start, content) in &windows {
        assert_eq!(*start, expected_start, "wrap segments are contiguous");
        expected_start = start + content.len();
    }
    assert_eq!(expected_start, 200, "every byte of the draft is on screen");
    for row in rows.iter().filter(|row| row.contains("jjj")) {
        assert!(!row.contains('…'), "no ellipsis in the composer: {row:?}");
    }
}

#[test]
fn wrap_rows_partition_and_respect_wide_cells() {
    // The pure geometry (unit level): budget counts display CELLS, wide
    // glyphs never straddle a wrap point, empty logical lines keep a row,
    // and an over-wide single grapheme still advances the walk.
    let rows = wrap_rows("漢漢漢漢漢", 5);
    // 2 glyphs (4 cells) per row: a third would need 6 cells.
    assert_eq!(
        rows.iter()
            .map(|row| (row.start, row.end, row.line_last))
            .collect::<Vec<_>>(),
        vec![(0, 6, false), (6, 12, false), (12, 15, true)]
    );
    // Logical lines wrap independently; empty lines keep their row.
    let rows = wrap_rows("abcd\n\nef", 3);
    assert_eq!(
        rows.iter()
            .map(|row| (row.start, row.end, row.line_last))
            .collect::<Vec<_>>(),
        vec![(0, 3, false), (3, 4, true), (5, 5, true), (6, 8, true)]
    );
    // A wide glyph at a 1-cell budget gets its own row (the walk always
    // advances — the terminal clips the overflow).
    assert_eq!(wrap_rows("漢漢", 1).len(), 2);
}

// ---- Item 2: autogrow cap + vertical window over VISUAL rows ----

#[test]
fn caret_stays_visible_beyond_the_autogrow_cap() {
    // MUTATION CHECK (caret-always-visible): in `composer_lines`, delete
    // the `if cursor_row_index < skip { skip = cursor_row_index; }` clamp
    // and the Home half of this test fails — the caret row scrolls out of
    // the tail window. Verified by revert.
    let mut model = session_model();
    // 520 cells at 90 cols (budget 85) → 7 visual rows, over the 5-row cap.
    for _ in 0..520 {
        model.handle(key(KeyCode::Char('x')));
    }
    let theme = model.theme.theme();
    let (rows, hits, terminal) = draw(&model, 90, 34);
    let windows = composer_windows(&hits);
    assert_eq!(windows.len(), 5, "the autogrow cap counts VISUAL rows");
    // Tail preferred: the head rows are scrolled out, marked ⋮, and the
    // caret (end of text) is visible on the last row.
    let first_y = windows[0].0.y;
    assert!(
        rows[first_y as usize].contains('⋮'),
        "hidden-above marker: {:?}",
        rows[first_y as usize]
    );
    let last = windows.last().expect("rows");
    let caret_x = last.0.x + u16::try_from(last.2.len() % 85).expect("fits");
    let buffer = terminal.backend().buffer();
    assert_eq!(
        buffer[(caret_x, last.0.y)].bg,
        Color::from(theme.gold),
        "end-of-text caret cell visible on the tail row"
    );
    // Home pulls the caret to byte 0 — the window follows it UP: the
    // first wrapped row (sigil, no ⋮) shows with the caret on it, and the
    // hidden tail is marked below.
    model.handle(key(KeyCode::Home));
    let (rows, hits, terminal) = draw(&model, 90, 34);
    let windows = composer_windows(&hits);
    assert_eq!(windows[0].1, 0, "the caret's row is in the window");
    let y = windows[0].0.y;
    assert!(rows[y as usize].contains("❯ xxx"), "head row with sigil");
    let buffer = terminal.backend().buffer();
    assert_eq!(
        buffer[(windows[0].0.x, y)].bg,
        Color::from(theme.gold),
        "caret cell on the first wrapped row"
    );
    let tail_y = windows.last().expect("rows").0.y;
    assert!(
        rows[tail_y as usize].contains('⋮'),
        "hidden-below marker on the window's last row"
    );
}

// ---- Item 3: ↑/↓ walk visual rows; Home/End stay logical ----

#[test]
fn arrows_walk_visual_rows_sticky_and_home_end_stay_logical() {
    let mut c = Composer::new();
    c.set_wrap_budget(10);
    c.insert_str(&format!("{}abcde", "abcdefghij".repeat(2)));
    // 25 cells / budget 10 → visual rows 0..10, 10..20, 20..25; caret at
    // the end (row 2, col 5).
    assert!(c.line_up(false), "row 2 → row 1");
    assert_eq!(c.cursor(), 15, "column-sticky within the VISUAL row");
    assert!(c.line_up(false), "row 1 → row 0");
    assert_eq!(c.cursor(), 5);
    assert!(
        !c.line_up(false),
        "the first VISUAL row is the history hook"
    );
    assert!(c.line_down(false));
    assert_eq!(c.cursor(), 15, "sticky column survives the round trip");
    // Home/End cross wrap points to the LOGICAL line edges (the
    // documented TUI6 pairing: arrows visual, jumps logical).
    c.line_home(false);
    assert_eq!(c.cursor(), 0, "Home = logical line start across the wrap");
    c.line_end_key(false);
    assert_eq!(c.cursor(), 25, "End = logical line end across the wrap");
    // Wide cells count as 2 within a visual row, exactly as they did on
    // logical lines.
    let mut c = Composer::new();
    c.set_wrap_budget(5);
    c.insert_str("漢漢漢漢漢");
    assert!(c.line_up(false), "row 2 → row 1 (wide)");
    assert_eq!(c.cursor(), 9, "col 2 lands after the first wide glyph");
    // A caret exactly ON a wrap point belongs to the FOLLOWING row (the
    // no-affinity law) — ↑ from it walks to the row above.
    let mut c = Composer::new();
    c.set_wrap_budget(10);
    c.insert_str("abcdefghijklmno");
    c.line_home(false);
    for _ in 0..10 {
        c.move_right(false);
    }
    assert_eq!(c.cursor(), 10, "caret on the wrap point");
    assert!(c.line_up(false), "…is on the SECOND visual row");
    assert_eq!(c.cursor(), 0);
}

#[test]
fn shift_arrows_extend_across_visual_rows() {
    let mut c = Composer::new();
    c.set_wrap_budget(10);
    c.insert_str(&format!("{}abcde", "abcdefghij".repeat(2)));
    // ⇧↑ from the end extends the selection up one VISUAL row.
    assert!(c.line_up(true));
    assert_eq!(c.selected_text(), Some("fghijabcde"));
    // ⇧↑ on the first visual row extends to the buffer start (TUI5.1
    // fix 4's outer-edge law, unchanged by the wrap).
    assert!(c.line_up(true));
    assert!(c.line_up(true));
    assert_eq!(c.cursor(), 0);
    assert_eq!(c.selected_text(), Some("abcdefghijabcdefghijabcde"));
    // ⇧↓ mirrors: one visual row down from the start.
    let mut c = Composer::new();
    c.set_wrap_budget(10);
    c.insert_str("abcdefghijklmno");
    c.line_home(false);
    assert!(c.line_down(true));
    assert_eq!(c.selected_text(), Some("abcdefghij"));
}

// ---- Item 3/5: clicks and drags map through the wrapped windows ----

#[test]
fn click_maps_through_the_wrapped_row_window() {
    // MUTATION CHECK (click-maps-through-wrapped-row): in
    // `composer_lines`, replace the window's `start: row.start` with
    // `start: 0` and this fails — a click on the second wrapped row would
    // place the caret in the first segment's bytes. Verified by revert.
    let mut model = session_model();
    for _ in 0..200 {
        model.handle(key(KeyCode::Char('x')));
    }
    let (_, hits, _) = draw(&model, 90, 34);
    let windows = composer_windows(&hits);
    assert!(windows.len() >= 2, "wrapped rows on screen");
    let (rect, start, _) = &windows[1];
    assert_eq!(*start, 85, "second row starts at the wrap point (90-5)");
    // Click 10 cells into the SECOND visual row.
    dispatch_input(
        &mut model,
        &hits,
        mouse(MouseEventKind::Down(MouseButton::Left), rect.x + 10, rect.y),
    );
    assert_eq!(
        model.composer.cursor(),
        95,
        "caret at the clicked grapheme of the second wrap segment"
    );
    // A drag from there onto the FIRST row selects backwards across the
    // wrap point through the same windows.
    dispatch_input(
        &mut model,
        &hits,
        mouse(
            MouseEventKind::Drag(MouseButton::Left),
            rect.x + 10,
            rect.y - 1,
        ),
    );
    assert_eq!(
        model.composer.selection_range(),
        Some((10, 95)),
        "drag maps through the row above's window"
    );
    dispatch_input(
        &mut model,
        &hits,
        mouse(
            MouseEventKind::Up(MouseButton::Left),
            rect.x + 10,
            rect.y - 1,
        ),
    );
}

// ---- Item 5: resize reflows — wrap points derive from the width ----

#[test]
fn resize_reflows_the_wrap_from_the_current_width() {
    let mut model = session_model();
    for _ in 0..120 {
        model.handle(key(KeyCode::Char('j')));
    }
    // 120 cells: 2 rows at 118 cols (budget 113), 3 rows at 60 (budget 55).
    let (_, hits, _) = draw(&model, 118, 34);
    assert_eq!(composer_windows(&hits).len(), 2);
    let (_, hits, _) = draw(&model, 60, 34);
    assert_eq!(composer_windows(&hits).len(), 3);
    // The MODEL stored no wrap state across those frames: text and caret
    // are untouched, only the derived geometry moved.
    assert_eq!(model.composer.text().len(), 120);
    assert_eq!(model.composer.cursor(), 120);
    // ↑ after the 60-col frame walks the 60-col rows (budget feedback):
    // caret col 10 on the tail row → col 10 of the middle row.
    model.handle(key(KeyCode::Up));
    assert_eq!(model.composer.cursor(), 65);
    // After re-rendering at 118 the SAME key walks the 118-col rows:
    // End puts the caret at 120 = col 7 of the 118-col tail row (rows
    // split at byte 113 now, not 55/110), so ↑ lands at col 7 of the row
    // ABOVE — wrap points came from the current width, not from anything
    // remembered.
    model.handle(key(KeyCode::End));
    let (_, hits, _) = draw(&model, 118, 34);
    assert_eq!(composer_windows(&hits).len(), 2);
    model.handle(key(KeyCode::Up));
    assert_eq!(
        model.composer.cursor(),
        7,
        "↑ at 118 cols walks the 118-col wrap geometry"
    );
}

// ---- Item 4: every composer surface wraps ----

#[test]
fn every_composer_surface_soft_wraps() {
    // The sim's InputBar is ONE `<textarea rows={1}>` with autoGrow for
    // EVERY surface — launcher, session, subagent, aura (tui.js:3004-3027,
    // placeholders switch per screen) — and a browser textarea soft-wraps.
    // The arg-slot is the same textarea with staged text (argSlots,
    // tui.js:203/2743), so NO composer surface is single-line: caret_window
    // has no survivors.
    // Launcher.
    let mut model = launcher_model();
    assert_eq!(model.screen, Screen::Launcher);
    for _ in 0..200 {
        model.handle(key(KeyCode::Char('w')));
    }
    let (rows, hits, _) = draw(&model, 90, 34);
    assert!(
        composer_windows(&hits).len() >= 2,
        "launcher composer wraps"
    );
    assert!(
        rows.iter()
            .filter(|row| row.contains("www"))
            .all(|row| !row.contains('…')),
        "no ellipsis on the launcher band"
    );
    // Aura.
    let mut model = launcher_model();
    for c in "/aura".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    assert_eq!(model.screen, Screen::Aura);
    for _ in 0..200 {
        model.handle(key(KeyCode::Char('v')));
    }
    let (rows, hits, _) = draw(&model, 90, 34);
    assert!(composer_windows(&hits).len() >= 2, "aura composer wraps");
    assert!(
        rows.iter()
            .filter(|row| row.contains("vvv"))
            .all(|row| !row.contains('…')),
        "no ellipsis on the aura band"
    );
    // Session (the owner's screenshot surface) is pinned throughout this
    // suite; the subagent band is covered by the item-6 sweep below.
}

// ---- TUI6.1 fix 3: the zero-width cluster costs one cell EVERYWHERE ----

#[test]
fn zero_width_leading_cluster_gets_one_cell_everywhere() {
    // Review r1 finding 3 (P2): draft "\u{301}a" (a combining acute
    // standing alone at the buffer start — reachable through paste) with
    // the caret at byte 0 painted NO caret cell (the cursor-styled
    // cluster occupied zero cells) while click mapping charged it one
    // invented cell, skewing the visible 'a'. The one policy
    // (composer::cluster_cells): the cluster costs ONE cell everywhere,
    // and the renderer gives it a space base.
    //
    // MUTATION CHECK (zero-width-cell render): drop the space-base
    // `run.push(' ')` from composer_row_spans and the caret-cell
    // assertion fails (no gold cell at the cluster's column). Verified
    // by revert.
    let mut model = session_model();
    model.handle(AppEvent::Paste("\u{301}a".to_owned()));
    model.handle(key(KeyCode::Home));
    assert_eq!(model.composer.cursor(), 0);
    let theme = model.theme.theme();
    let (_, hits, terminal) = draw(&model, 90, 34);
    // Anchor on the composer's OWN row via its hit window (the transcript
    // also draws ❯ prompt rows above the band).
    let windows = composer_windows(&hits);
    let rect = windows[0].0;
    let buffer = terminal.backend().buffer();
    // The caret is PAINTED: one real cell, gold ground, at the band's
    // first content column.
    assert_eq!(
        buffer[(rect.x, rect.y)].bg,
        Color::from(theme.gold),
        "caret cell painted on the zero-width cluster"
    );
    assert_eq!(
        buffer[(rect.x + 1, rect.y)].symbol(),
        "a",
        "the 'a' sits one cell right"
    );
    dispatch_input(
        &mut model,
        &hits,
        mouse(MouseEventKind::Down(MouseButton::Left), rect.x + 1, rect.y),
    );
    assert_eq!(
        model.composer.cursor(),
        "\u{301}".len(),
        "clicking the 'a' cell lands AFTER the one-cell cluster"
    );
    dispatch_input(
        &mut model,
        &hits,
        mouse(MouseEventKind::Up(MouseButton::Left), rect.x + 1, rect.y),
    );
    let (_, hits, _) = draw(&model, 90, 34);
    let rect = composer_windows(&hits)[0].0;
    dispatch_input(
        &mut model,
        &hits,
        mouse(MouseEventKind::Down(MouseButton::Left), rect.x, rect.y),
    );
    assert_eq!(
        model.composer.cursor(),
        0,
        "clicking the cluster cell lands at 0"
    );
    dispatch_input(
        &mut model,
        &hits,
        mouse(MouseEventKind::Up(MouseButton::Left), rect.x, rect.y),
    );
}

#[test]
fn cluster_families_price_and_wrap_consistently() {
    // The reviewer's unexercised families (decomposed marks, flags, skin
    // tones, standalone marks after a newline), headless over wrapped
    // rows, all priced through the ONE policy.
    //
    // MUTATION CHECK (one-price law): drop the `.max(1)` from
    // `cluster_cells` and the standalone-mark wrap pin fails (the wrap
    // walk would charge the mark zero cells while click mapping charges
    // one — the exact disagreement of review r1 finding 3). Verified by
    // revert.
    use haider_tui::composer::{byte_at_col, cluster_cells, cluster_cols, visual_row_of};
    use unicode_segmentation::UnicodeSegmentation;
    assert_eq!(cluster_cells("\u{301}"), 1, "standalone mark: one cell");
    assert_eq!(cluster_cells("e\u{301}"), 1, "decomposed é: one cell");
    assert_eq!(cluster_cells("\u{1F1E6}\u{1F1E7}"), 2, "flag pair");
    assert_eq!(cluster_cells("👍\u{1F3FD}"), 2, "skin-tone thumbs-up");
    // The standalone-mark wrap pin: budget 3 packs mark+a+b on row 0.
    let text = "\u{301}abcde";
    let rows = wrap_rows(text, 3);
    assert_eq!(
        rows[0].end, 4,
        "row 0 = mark(1 cell) + ab, ending at byte 4"
    );
    // byte_at_col agrees with the wrap pricing on the same content.
    assert_eq!(byte_at_col(&text[rows[0].start..rows[0].end], 0), 0);
    assert_eq!(byte_at_col(&text[rows[0].start..rows[0].end], 1), 2);
    // Families over wrapped rows: partition + boundary law at every
    // budget, standalone marks after newlines included.
    for text in [
        format!("{}tail", "e\u{301}".repeat(20)),
        format!("{}x", "\u{1F1E6}\u{1F1E7}".repeat(12)),
        format!("{}y", "👍\u{1F3FD}".repeat(12)),
        format!("head\n\u{301}{}", "e\u{301}".repeat(15)),
    ] {
        for budget in 2..12 {
            let rows = wrap_rows(&text, budget);
            for row in &rows {
                assert!(
                    text.grapheme_indices(true).any(|(i, _)| i == row.start)
                        || row.start == text.len(),
                    "mid-cluster row start {} in {text:?} at budget {budget}",
                    row.start
                );
                assert!(
                    cluster_cols(&text[row.start..row.end]) <= budget.max(2),
                    "row over budget at {budget}"
                );
            }
            // Sticky navigation lands on cluster boundaries at this
            // budget too.
            let mut c = Composer::new();
            c.set_wrap_budget(budget);
            c.insert_str(&text);
            let rows_now = c.visual_rows();
            let last = visual_row_of(&rows_now, c.cursor());
            if last > 0 {
                assert!(c.line_up(false));
                assert!(
                    text.grapheme_indices(true).any(|(i, _)| i == c.cursor())
                        || c.cursor() == text.len(),
                    "↑ landed mid-cluster at budget {budget}"
                );
            }
        }
    }
}

// ---- TUI6.2 fix 1: the sticky column dies with the budget that minted it ----

#[test]
fn sticky_column_is_invalidated_by_a_budget_change() {
    // Review r2 finding 1, the reviewer's exact repro: budget 13, caret
    // 4, Down → 17 (sticky col 4 cached); resize to budget 5; Down with
    // the STALE column walked to byte 24 — current geometry (caret 17 =
    // col 2 of row [15,20)) lands 22. The column is mint-tagged with its
    // budget and re-derived on mismatch.
    //
    // MUTATION CHECK (sticky-mint-tag): make `sticky_col_for` trust the
    // cached column regardless of mint (`Some((_, col)) => col`) and this
    // fails with 24. Verified by revert.
    let mut model = session_model();
    for c in "abcdefghijklmnopqrstuvwxyz1234".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    let (_, hits, _) = draw(&model, 18, 30); // budget 13
    model.handle(key(KeyCode::Home));
    for _ in 0..4 {
        model.handle(key(KeyCode::Right));
    }
    model.handle(key(KeyCode::Down));
    assert_eq!(model.composer.cursor(), 17, "col 4 sticky under budget 13");
    dispatch_input(&mut model, &hits, Event::Resize(10, 30)); // budget 5
    model.handle(key(KeyCode::Down));
    assert_eq!(
        model.composer.cursor(),
        22,
        "the stale col-4 cache would land 24; the re-minted col 2 lands 22"
    );
    // ⇧-extension shares the path: ⇧↓ from 22 extends with the FRESH col.
    model.handle(key_mod(KeyCode::Down, KeyModifiers::SHIFT));
    assert_eq!(model.composer.cursor(), 27);
    assert_eq!(model.composer.selection_range(), Some((22, 27)));
}

#[test]
fn parked_draft_sticky_column_dies_with_its_budget() {
    // The reviewer's parked variant: the stale column traveled inside
    // the parked draft and produced the same byte 24. The mint tag kills
    // it on wake exactly as it does live.
    let mut model = launcher_model();
    for c in "abcdefghijklmnopqrstuvwxyz1234".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    let (_, hits, _) = draw(&model, 18, 30); // budget 13
    model.handle(key(KeyCode::Home));
    for _ in 0..4 {
        model.handle(key(KeyCode::Right));
    }
    model.handle(key(KeyCode::Down));
    assert_eq!(model.composer.cursor(), 17, "sticky (13, 4) minted");
    // Park under the launcher key; resize while away; come back.
    let id = common::session_named(&model, "billing-service");
    model.handle_hit(Hit::AttachSession(id));
    dispatch_input(&mut model, &hits, Event::Resize(10, 30));
    model.handle(key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert_eq!(model.screen, Screen::Launcher);
    model.handle(key(KeyCode::Down));
    assert_eq!(
        model.composer.cursor(),
        22,
        "the parked (13, 4) mint is dead under budget 5 — re-derived col 2"
    );
}

// ---- TUI6.2 fix 2: every render branch publishes the width ----

#[test]
fn fresh_empty_render_publishes_its_width() {
    // Review r2 finding 2, the reviewer's exact repro: a fresh EMPTY
    // composer rendered at width 18 kept budget 0 (the empty branch
    // returned before the publish), so type + queued Home, Right×4, Down
    // before any redraw walked LOGICAL lines — Down on a single logical
    // line is the history hook, leaving the cursor at 4. Budget 13's
    // wrapped rows land 17.
    //
    // MUTATION CHECK (publish-before-return): move the
    // `set_wrap_budget` publish in `composer_lines` back below the
    // empty-composer return and this fails with cursor 4. Verified by
    // revert.
    let mut model = launcher_model();
    let (_, _, _) = draw(&model, 18, 30); // EMPTY composer frame
    for c in "abcdefghijklmnopqrstuvwxyz1234".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    // All queued BEFORE the next redraw.
    model.handle(key(KeyCode::Home));
    for _ in 0..4 {
        model.handle(key(KeyCode::Right));
    }
    model.handle(key(KeyCode::Down));
    assert_eq!(
        model.composer.cursor(),
        17,
        "the empty frame published budget 13; queued nav wraps"
    );
}

// ---- TUI6.2 fix 3: the surface-switch authority ----

#[test]
fn chip_close_from_aura_swaps_the_draft() {
    // Review r2 finding 3, the reviewer's exact leak: a background chip
    // close while the user sat in AURA assigned Screen::Session
    // directly — the aura draft crossed keys unswapped and could be
    // SUBMITTED on the session surface. Through switch_surface the aura
    // draft parks under its own key and the session's draft comes live.
    //
    // MUTATION CHECK (switch-authority): revert close_chip_state to
    // `self.screen = Screen::Session;` and this fails — the session
    // composer contains "aura draft". Verified by revert.
    let mut model = launcher_model();
    model.chips = vec![ChipModel::from_seed(ChipSeed {
        agent: "t1-docs".to_owned(),
        parent: None,
        ros: None,
        callsign: "Husayn".to_owned(),
        hon: "(r)",
        full: "Husayn ibn Ali".to_owned(),
        name: "docs".to_owned(),
        model: "fable-5".to_owned(),
        device: "macbook".to_owned(),
        state: ChipDisplayState::Running,
        tokens: 100,
        prefill: Vec::new(),
    })];
    for c in "/aura".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    assert_eq!(model.screen, Screen::Aura);
    for c in "aura draft".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    // The background close fires while the aura screen is up.
    model.close_chip_state("t1-docs");
    assert_eq!(model.screen, Screen::Session);
    assert!(
        !model.composer.text().contains("aura draft"),
        "the aura draft must NOT be submittable on the session surface: {:?}",
        model.composer.text()
    );
    // The draft parked under ITS key: returning to aura recalls it.
    for c in "/aura".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    assert_eq!(model.screen, Screen::Aura);
    assert_eq!(model.composer.text(), "aura draft");
}

// ---- TUI6.2 fix 5: the login card returns what it borrowed ----

#[test]
fn login_close_restores_the_draft_and_its_history() {
    // Review r2 finding 5: /login's stash had no paired restore — Esc
    // and ⌃C only cleared the card, stranding the parked composer AND
    // its input-history ring (the r1-era safe-adjudication covered text
    // only). Both close paths now restore.
    //
    // MUTATION CHECK (login-close-restore): drop the `restore_draft()`
    // from login_key's Esc arm and this fails — history_prev on the
    // post-close composer recalls nothing. Verified by revert.
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    for c in "remember this".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter)); // live submit: ring records it
    for c in "/login anthropic api".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Esc));
    model.handle(key(KeyCode::Enter));
    assert!(
        model.login.is_some(),
        "card open (flash: {:?})",
        model.flash
    );
    // The queued TUI6.2 empty-draft pin: the draft TEXT is empty by
    // construction at open (the /login submit consumed it) — the ring is
    // what the stash protects.
    assert!(
        model.composer.is_empty(),
        "scratch composer while card open"
    );
    model.handle(key(KeyCode::Esc)); // close: the card returns the band
    assert!(model.login.is_none());
    assert!(
        model.composer.history_prev(),
        "the ring came back with the restored draft"
    );
    // The ring holds the canonical /login form and the submit before it.
    assert!(model.composer.history_prev());
    assert_eq!(model.composer.text(), "remember this");
}

#[test]
fn login_card_is_modal_against_the_hits_beneath_it() {
    // TUI6.2b (the interrupted verifier's dying finding, confirmed): with
    // the login card open on the launcher, the launcher's AttachSession
    // hit rects stayed LIVE underneath the modal. Clicking one ran
    // open_session mid-login: its stash parked the empty scratch over
    // the login-parked launcher draft (destroying the ring), checkout
    // flipped the screen under the still-open card, and the card's later
    // Esc-restore then clobbered the session draft with an empty one —
    // three corruptions from one click. The card owns the KEYS already
    // (login_key); it now owns the HITS too: handle_hit swallows
    // everything while the card is open.
    //
    // MUTATION CHECK (login-modal-hits): drop the `login.is_some()` gate
    // from handle_hit and this fails — the click attaches a session
    // under the card. Verified by revert.
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    for c in "remember this".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter)); // ring records it
    for c in "/login anthropic api".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Esc));
    model.handle(key(KeyCode::Enter));
    assert!(model.login.is_some(), "card open");
    // The frame under the card still lists the recent sessions; a click
    // on one of their rects must be swallowed by the modal.
    let (_, hits, _) = draw(&model, 100, 30);
    let attach = hits
        .iter()
        .find(|(_, hit)| matches!(hit, Hit::AttachSession(_)))
        .map(|(rect, _)| *rect);
    if let Some(rect) = attach {
        dispatch_input(
            &mut model,
            &hits,
            mouse(MouseEventKind::Down(MouseButton::Left), rect.x + 2, rect.y),
        );
        dispatch_input(
            &mut model,
            &hits,
            mouse(MouseEventKind::Up(MouseButton::Left), rect.x + 2, rect.y),
        );
    }
    assert_eq!(
        model.screen,
        Screen::Launcher,
        "no surface change under the card"
    );
    assert!(model.login.is_some(), "the card is still open");
    // The parked draft survived: closing the card restores the ring.
    model.handle(key(KeyCode::Esc));
    assert!(model.composer.history_prev(), "ring intact after the modal");
}

// ---- TUI6.2c: total login modality (the re-spawned verifier's finds) ----

#[test]
fn async_session_open_under_the_login_card_aborts_it_and_keeps_the_ring() {
    // Verifier finding 1 (P1): on the live launcher, submit races /login
    // — the daemon's Created reply runs open_session UNDER the open card
    // (a driver call the hit gate never sees). Pre-fix: the checkout's
    // stash parked the login scratch OVER the login-parked launcher
    // draft (ring destroyed), the screen flipped under the card, and the
    // card's Esc-restore restored nothing. The stash chokepoint now
    // aborts the card (secret wiped) and returns the borrowed band FIRST.
    //
    // MUTATION CHECK (login-switch-safety): remove the login-abort block
    // from stash_draft and this fails — the ring is destroyed. Verified
    // by revert.
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    for c in "remember this".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter)); // CreateSession in flight
    for c in "/login anthropic api".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Esc));
    model.handle(key(KeyCode::Enter));
    assert!(
        model.login.is_some(),
        "card open while the reply is pending"
    );
    // The daemon replies: the driver calls open_session — the verifier's
    // exact route (live.rs LiveReply::Created).
    let id = common::session_named(&model, "billing-service");
    model.open_session(&id);
    assert_eq!(model.screen, Screen::Session);
    assert!(
        model.login.is_none(),
        "the surface switch ABORTS the card — it cannot float over a \
         surface it never borrowed"
    );
    // The launcher draft's ring survived the abort: back on the
    // launcher, recall works.
    model.handle(key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert_eq!(model.screen, Screen::Launcher);
    assert!(
        model.composer.history_prev(),
        "the ring survived the async switch under the card"
    );
}

#[test]
fn background_chip_close_under_the_login_card_aborts_it() {
    // Verifier finding 2 (P1), their exact route: the card opened on the
    // AURA composer ("/login anthropic api " — trailing space, since Esc
    // exits aura instead of dismissing the palette), then the demo
    // driver's background chip close fired switch_surface(Session) under
    // it. Pre-fix: the aura→session KEY change hauled the parked draft
    // live under the modal, the card's close then destroyed it, and the
    // aura ring died. The stash chokepoint aborts the card on the
    // key-changing switch. (A SAME-key flip — e.g. a chip close while on
    // the launcher scratch — swaps nothing and coherently keeps the card:
    // the corruption class is key change, and the chokepoint rides the
    // stash that only key changes perform.)
    let mut model = launcher_model();
    model.chips = vec![ChipModel::from_seed(ChipSeed {
        agent: "t1-docs".to_owned(),
        parent: None,
        ros: None,
        callsign: "Husayn".to_owned(),
        hon: "(r)",
        full: "Husayn ibn Ali".to_owned(),
        name: "docs".to_owned(),
        model: "fable-5".to_owned(),
        device: "macbook".to_owned(),
        state: ChipDisplayState::Running,
        tokens: 100,
        prefill: Vec::new(),
    })];
    for c in "/aura".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    assert_eq!(model.screen, Screen::Aura);
    for c in "remember this".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter)); // aura submit: the ring records it
    for c in "/login anthropic api ".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    assert!(model.login.is_some(), "card open on the aura surface");
    model.close_chip_state("t1-docs"); // the background close, mid-login
    assert_eq!(model.screen, Screen::Session);
    assert!(
        model.login.is_none(),
        "card aborted by the key-changing switch"
    );
    assert!(
        !model.composer.text().contains("remember"),
        "no aura draft hauled onto the session under the modal"
    );
    // The aura ring survived, parked under its own key.
    for c in "/aura".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    assert_eq!(model.screen, Screen::Aura);
    assert!(model.composer.history_prev(), "aura ring intact");
}

#[test]
fn login_card_outranks_an_arriving_menu_on_the_band() {
    // Verifier finding 3 (P1): a MenuOpened envelope stole the card's
    // FACE while the card kept the KEYBOARD — the band rendered the menu,
    // so pressing `1` for the visible option landed in the masked secret
    // and Enter staged a garbage credential. The card now outranks the
    // menu on the band: the menu waits, unrendered and unclickable.
    //
    // MUTATION CHECK (card-outranks-menu): drop the `model.login` gate on
    // the session ledger's `menu` binding and this fails — the option
    // rows render and emit hits under the card. Verified by revert.
    let mut model = session_model();
    model.mode = RuntimeMode::Live;
    for c in "/login anthropic api".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Esc));
    model.handle(key(KeyCode::Enter));
    assert!(
        model.login.is_some(),
        "card open (flash: {:?})",
        model.flash
    );
    model.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(
        haider_protocol::menu::Menu {
            id: haider_protocol::ids::MenuId::new("perm-1"),
            kind: haider_protocol::menu::MenuKind::Choice,
            title: "Allow fs_patch — main.rs?".to_owned(),
            body: vec![],
            options: [("allow", "Allow once"), ("deny", "Deny")]
                .iter()
                .map(|(key, label)| haider_protocol::menu::MenuOption {
                    key: (*key).to_owned(),
                    label: (*label).to_owned(),
                    detail: None,
                    decision: None,
                })
                .collect(),
            blocking: true,
            scope: haider_protocol::menu::MenuScope::Session,
            origin: "permission".to_owned(),
            ttl_ms: None,
            timeout_option: None,
        },
    ))));
    assert!(model.projection.open_menu().is_some(), "menu queued");
    let (rows, hits, _) = draw(&model, 100, 30);
    assert!(
        rows.iter().any(|row| row.contains("API key")),
        "the band shows the CARD, not the menu"
    );
    assert!(
        !rows.iter().any(|row| row.contains("Allow once")),
        "the menu waits unrendered: {rows:?}"
    );
    assert!(
        !hits
            .iter()
            .any(|(_, hit)| matches!(hit, Hit::MenuOption { .. })),
        "and unclickable"
    );
    // Keystrokes stay the card's: `1` extends the mask, answers nothing.
    model.handle(key(KeyCode::Char('1')));
    assert!(model.outbox.is_empty(), "no menu answer from a secret byte");
    // The card closes → the menu takes the band on the next frame.
    model.handle(key(KeyCode::Esc));
    let (rows, _, _) = draw(&model, 100, 30);
    assert!(rows.iter().any(|row| row.contains("Allow once")));
}

#[test]
fn demo_driver_login_close_restores_through_the_one_method() {
    // Verifier finding 5 (P2): the demo driver's LoginApi arm closed the
    // card with a bare `login = None`, stranding the parked draft and
    // ring (restore_draft is private to the model — by design). The
    // model-owned close_login_card is now the ONE close path; the driver
    // routes through it.
    //
    // MUTATION CHECK (one-close-method): revert the driver arm to
    // `model.login = None` and this fails — history_prev recalls
    // nothing after the demo close. Verified by revert.
    let mut model = launcher_model();
    for c in "remember this".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    model.requests.clear();
    for c in "/login anthropic api".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Esc));
    model.handle(key(KeyCode::Enter));
    assert!(model.login.is_some());
    for c in "sk-demo".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter)); // stages LoginApi
    // The demo driver answers the request (runtime.rs LoginApi arm).
    let (mut driver, _rx) = common::driver_for(&model);
    common::drain(&mut driver, &mut model);
    assert!(
        model.login.is_none(),
        "the demo declined and closed the card"
    );
    assert!(
        model.composer.history_prev(),
        "the ring came back through close_login_card"
    );
}

#[test]
fn modals_own_hover_and_wheel_too() {
    // Verifier findings 4 + 7 (P2/P3): hover moved menu selections and
    // wheel scrolled beneath the open card — the remaining un-gated
    // input routes after the TUI6.2b hit gate. Both now gate on the
    // modal exactly as hits and keys do.
    let mut model = session_model();
    model.mode = RuntimeMode::Live;
    for c in "/login anthropic api".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Esc));
    model.handle(key(KeyCode::Enter));
    assert!(model.login.is_some());
    // A REAL open menu waits behind the card (finding 3's setup) — the
    // hover arm's own validity check passes for it, so only the modal
    // gate stands between the hover and the selection.
    model.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(
        haider_protocol::menu::Menu {
            id: haider_protocol::ids::MenuId::new("perm-1"),
            kind: haider_protocol::menu::MenuKind::Choice,
            title: "Allow fs_patch — main.rs?".to_owned(),
            body: vec![],
            options: [("allow", "Allow once"), ("deny", "Deny")]
                .iter()
                .map(|(key, label)| haider_protocol::menu::MenuOption {
                    key: (*key).to_owned(),
                    label: (*label).to_owned(),
                    detail: None,
                    decision: None,
                })
                .collect(),
            blocking: true,
            scope: haider_protocol::menu::MenuScope::Session,
            origin: "permission".to_owned(),
            ttl_ms: None,
            timeout_option: None,
        },
    ))));
    assert_eq!(model.menu_selection, 0);
    model.handle_hover(Some(Hit::MenuOption {
        menu: haider_protocol::ids::MenuId::new("perm-1"),
        index: 1,
    }));
    assert_eq!(model.menu_selection, 0, "hover gated under the card");
    let scroll = model.scroll_back.get();
    model.handle_wheel(true);
    assert_eq!(
        model.scroll_back.get(),
        scroll,
        "wheel gated under the card"
    );
}

#[test]
fn chip_view_scroll_never_carries_onto_the_session() {
    // Verifier finding 8 (P3): esc/crumb from the chip view kept the
    // chip transcript's scroll offset on the session (⌂ and ChipRow
    // already reset). All exits now match.
    let mut model = subagent_model();
    let (_, _, _) = draw(&model, 90, 20); // establish a scroll range
    model.scroll_back.set(9);
    model.handle(key(KeyCode::Esc));
    assert_eq!(model.screen, Screen::Session);
    assert_eq!(
        model.scroll_back.get(),
        0,
        "esc resets the carried chip-view scroll"
    );
}

// ---- TUI6.2 promoted seam pins (the verifier's s1-s6, now shipped) ----
//
// MUTATION CHECK (budget-across-swap, the whole group): the s1/s2/s3/s6
// pins die together when `restore_draft`'s `set_wrap_budget` carry is
// dropped (the TUI6.1b revert, re-executed against this group). s4 dies
// with fix 5's restore; s5 rides the fix-2 publish. Verified by revert.

#[test]
fn s1_reverse_seam_session_draft_wakes_at_current_width() {
    let mut model = launcher_model();
    let id = common::session_named(&model, "billing-service");
    model.handle_hit(Hit::AttachSession(id.clone()));
    assert_eq!(model.screen, Screen::Session);
    for c in "abcdefghijklmnopqrst".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    let (_, hits, _) = draw(&model, 20, 30); // budget 15
    model.handle(key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert_eq!(model.screen, Screen::Launcher);
    dispatch_input(&mut model, &hits, Event::Resize(10, 30)); // budget 5
    model.handle_hit(Hit::AttachSession(id));
    assert_eq!(model.screen, Screen::Session);
    assert_eq!(model.composer.text(), "abcdefghijklmnopqrst");
    model.handle(key(KeyCode::Up)); // queued before any redraw
    assert_eq!(
        model.composer.cursor(),
        14,
        "the re-attached session draft walks budget 5, not its parked 15"
    );
}

#[test]
fn s2_aura_draft_wakes_at_current_width() {
    let mut model = launcher_model();
    for c in "/aura".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    for c in "abcdefghijklmnopqrst".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    let (_, hits, _) = draw(&model, 20, 30); // budget 15
    model.handle(key(KeyCode::Esc)); // park the aura draft
    assert_eq!(model.screen, Screen::Launcher);
    dispatch_input(&mut model, &hits, Event::Resize(10, 30)); // budget 5
    for c in "/aura".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    assert_eq!(model.composer.text(), "abcdefghijklmnopqrst");
    model.handle(key(KeyCode::Up)); // queued before any redraw
    assert_eq!(model.composer.cursor(), 14, "aura instance, same seam law");
}

#[test]
fn s3_double_resize_while_parked_last_width_wins() {
    let mut model = launcher_model();
    for c in "abcdefghijklmnopqrst".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    let (_, hits, _) = draw(&model, 20, 30);
    let id = common::session_named(&model, "billing-service");
    model.handle_hit(Hit::AttachSession(id));
    dispatch_input(&mut model, &hits, Event::Resize(40, 30)); // budget 35
    dispatch_input(&mut model, &hits, Event::Resize(12, 30)); // budget 7
    model.handle(key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert_eq!(model.screen, Screen::Launcher);
    model.handle(key(KeyCode::Up)); // queued before any redraw
    assert_eq!(
        model.composer.cursor(),
        13,
        "the LAST width (budget 7: caret 20 = col 6 of [14,20]) wins — ↑ \
         lands col 6 of [7,14)"
    );
}

#[test]
fn s4_login_seam_resize_while_card_open() {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    for c in "/login anthropic api".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Esc));
    model.handle(key(KeyCode::Enter));
    assert!(model.login.is_some());
    let (_, hits, _) = draw(&model, 20, 30);
    dispatch_input(&mut model, &hits, Event::Resize(10, 30)); // while the card owns the band
    model.handle(key(KeyCode::Esc)); // close restores (fix 5)
    for c in "abcdefghijklmnopqrst".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Up)); // queued before any redraw
    assert_eq!(
        model.composer.cursor(),
        14,
        "the restored draft navigates at the width resized-to under the card"
    );
}

#[test]
fn s5_fresh_draft_wakes_at_current_width() {
    let mut model = launcher_model();
    let (_, hits, _) = draw(&model, 20, 30);
    dispatch_input(&mut model, &hits, Event::Resize(10, 30));
    // A NEVER-visited surface: attach mints a fresh unwrap_or_default
    // draft — it must speak the current width immediately.
    let id = common::session_named(&model, "billing-service");
    model.handle_hit(Hit::AttachSession(id));
    assert_eq!(model.screen, Screen::Session);
    for c in "abcdefghijklmnopqrst".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Up)); // queued before any redraw
    assert_eq!(model.composer.cursor(), 14, "fresh draft, budget 5 rows");
}

#[test]
fn s6_reset_purge_restores_at_current_width() {
    let mut model = launcher_model();
    for c in "abcdefghijklmnopqrst".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    let (_, hits, _) = draw(&model, 20, 30);
    let id = common::session_named(&model, "billing-service");
    model.handle_hit(Hit::AttachSession(id));
    dispatch_input(&mut model, &hits, Event::Resize(10, 30));
    for c in "/reset".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Esc));
    model.handle(key(KeyCode::Enter));
    assert_eq!(model.screen, Screen::Launcher);
    assert_eq!(model.composer.text(), "abcdefghijklmnopqrst");
    model.handle(key(KeyCode::Up)); // queued before any redraw
    assert_eq!(
        model.composer.cursor(),
        14,
        "the reset-surviving launcher draft walks the current width"
    );
}

// ---- TUI6.1 fix 1: resize can never serve the previous frame's layout ----

#[test]
fn restored_draft_wakes_to_the_current_width_not_its_parked_one() {
    // TUI6.1 fix 1 closure, the stash-seam half (self-found while
    // closing review r1's class): a parked draft's wrap-budget Cell is
    // as old as ITS last render. Repro: draft 20 chars on the LAUNCHER
    // at 20 cols (budget 15), attach a session (draft parks), resize to
    // 10 cols while attached, ⌃C back to the launcher, then ↑ QUEUED
    // before any redraw — pre-closure the ↑ walked the parked 15-cell
    // rows; the law walks the current 10-col rows (budget 5).
    //
    // MUTATION CHECK (budget-across-swap): drop the
    // `set_wrap_budget(current_budget)` line from restore_draft and this
    // fails with cursor 5 (the parked 15-cell geometry: 20 → row 1
    // col 5) instead of 14 (5-cell). Verified by revert.
    let mut model = launcher_model();
    for c in "abcdefghijklmnopqrst".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    let (_, hits, _) = draw(&model, 20, 30);
    // Park the launcher draft by attaching a session (the digit shortcut
    // needs an EMPTY composer, so attach through the row's hit).
    let id = common::session_named(&model, "billing-service");
    model.handle_hit(Hit::AttachSession(id));
    assert_eq!(model.screen, Screen::Session);
    // The terminal resizes while the launcher draft is parked.
    dispatch_input(&mut model, &hits, Event::Resize(10, 30));
    // ⌃C: back to the launcher — the draft returns…
    model.handle(key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert_eq!(model.screen, Screen::Launcher);
    assert_eq!(model.composer.text(), "abcdefghijklmnopqrst");
    // …and a ↑ QUEUED before any redraw must walk the CURRENT width's
    // rows. 10-col geometry (budget 5): rows [0,5) [5,10) [10,15)
    // [15,20]; the caret at 20 sits at col 5 of the last row, so ↑ seeks
    // col 5 in [10,15) — past the wrap row's width, clamping to its last
    // grapheme start, byte 14. The parked 15-cell geometry (rows [0,15)
    // [15,20]) would land ↑ at byte 5 instead — the mutation check's
    // expected failure value.
    model.handle(key(KeyCode::Up));
    assert_eq!(
        model.composer.cursor(),
        14,
        "the restored draft walks the CURRENT 10-col geometry"
    );
}

#[test]
fn resize_reflows_navigation_before_any_queued_key() {
    // Review r1 finding 1 (P1), the reviewer's exact repro: render at 20
    // cols (budget 15) with the caret at byte 4, dispatch a resize to 10
    // cols, then a Down that RACES the redraw. Pre-fix the Down walked
    // the 15-cell rows and landed at byte 19; the law lands it at byte 9
    // (10-col geometry, budget 5). Reflow-before-input: the dispatch
    // seam applies the new width's budget on the Resize event itself.
    //
    // MUTATION CHECK (reflow-before-input): delete the
    // `set_wrap_budget(composer_text_budget(cols))` line from
    // dispatch_input's Resize arm and this fails with cursor 19.
    // Verified by revert.
    let mut model = session_model();
    for c in "abcdefghijklmnopqrst".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    let (_, hits, _) = draw(&model, 20, 30);
    model.handle(key(KeyCode::Home));
    for _ in 0..4 {
        model.handle(key(KeyCode::Right));
    }
    assert_eq!(model.composer.cursor(), 4);
    dispatch_input(&mut model, &hits, Event::Resize(10, 30));
    // The Down is queued BEFORE any redraw at the new size.
    dispatch_input(
        &mut model,
        &hits,
        Event::Key(ratatui::crossterm::event::KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )),
    );
    assert_eq!(
        model.composer.cursor(),
        9,
        "Down after resize walks the CURRENT width's rows (budget 5), \
         never the stale 20-col geometry (which lands 19)"
    );
}

#[test]
fn resize_retires_the_previous_frames_composer_hits() {
    // Review r1 finding 1 (P1), click half: resize bumps no text
    // revision, so the TUI5 stale-hit guard ACCEPTED clicks stamped by
    // pre-resize frames and mapped them through the old wrap windows.
    // The geometry epoch retires them whole: a stale click moves
    // nothing; the next frame's map works.
    //
    // MUTATION CHECK (geometry-epoch gate): drop the
    // `epoch != self.geometry_epoch.get()` clause from composer_press
    // (and composer_byte_at) and this fails — the stale click lands at
    // byte 89. Verified by revert.
    let mut model = session_model();
    for _ in 0..200 {
        model.handle(key(KeyCode::Char('x')));
    }
    let (_, stale_hits, _) = draw(&model, 90, 34);
    let stale = composer_windows(&stale_hits);
    let (stale_rect, _, _) = stale[1];
    let before = model.composer.cursor();
    dispatch_input(&mut model, &stale_hits, Event::Resize(60, 34));
    // The old frame's second-row click races the redraw: DROPPED whole.
    dispatch_input(
        &mut model,
        &stale_hits,
        mouse(
            MouseEventKind::Down(MouseButton::Left),
            stale_rect.x + 4,
            stale_rect.y,
        ),
    );
    assert_eq!(
        model.composer.cursor(),
        before,
        "a click stamped by pre-resize geometry never places a caret"
    );
    assert!(!model.composer_drag, "and never arms a drag");
    // A drag armed BEFORE the resize maps through nothing after it.
    let (_, hits, _) = draw(&model, 90, 34);
    let rows = composer_windows(&hits);
    dispatch_input(
        &mut model,
        &hits,
        mouse(
            MouseEventKind::Down(MouseButton::Left),
            rows[1].0.x + 4,
            rows[1].0.y,
        ),
    );
    assert_eq!(model.composer.cursor(), 89, "fresh press lands");
    dispatch_input(&mut model, &hits, Event::Resize(60, 34));
    dispatch_input(
        &mut model,
        &hits,
        mouse(
            MouseEventKind::Drag(MouseButton::Left),
            rows[0].0.x + 2,
            rows[0].0.y,
        ),
    );
    assert_eq!(
        model.composer.cursor(),
        89,
        "a drag across a resize maps through NO stale row — the caret stays"
    );
    dispatch_input(
        &mut model,
        &hits,
        mouse(
            MouseEventKind::Up(MouseButton::Left),
            rows[0].0.x + 2,
            rows[0].0.y,
        ),
    );
    // After a REDRAW at the new size, the fresh map works end to end.
    let (_, hits, _) = draw(&model, 60, 34);
    let rows = composer_windows(&hits);
    assert_eq!(rows[1].1, 55, "60-col wrap point (budget 55)");
    dispatch_input(
        &mut model,
        &hits,
        mouse(
            MouseEventKind::Down(MouseButton::Left),
            rows[1].0.x + 4,
            rows[1].0.y,
        ),
    );
    assert_eq!(model.composer.cursor(), 59, "post-redraw click lands");
    dispatch_input(
        &mut model,
        &hits,
        mouse(
            MouseEventKind::Up(MouseButton::Left),
            rows[1].0.x + 4,
            rows[1].0.y,
        ),
    );
}

// ---- Item 6: the band-anatomy sweep — two rules on EVERY input band ----

/// A screen row that reads as a horizontal rule.
fn is_rule(row: &str) -> bool {
    row.chars().filter(|c| *c == '─').count() >= 20
}

/// TUI6 item 6 (per Claude Code's own TUI): the input band carries a rule
/// ABOVE and a rule BELOW on every surface with an input. `first` finds
/// the band's first rendered row, `last` its last (the same needle for a
/// one-row band); the closing rule must land within the pad rows beneath.
fn assert_two_rules(rows: &[String], first: &str, last: &str, surface: &str) -> usize {
    let top = row_of(rows, first) as usize;
    let bottom = row_of(rows, last) as usize;
    assert!(
        is_rule(&rows[top - 1]),
        "{surface}: rule ABOVE the band, got {:?}",
        rows[top - 1]
    );
    let below = (1..=3)
        .map(|d| bottom + d)
        .find(|y| *y < rows.len() && is_rule(&rows[*y]));
    below.unwrap_or_else(|| {
        panic!(
            "{surface}: closing rule BELOW the band, got {:?}",
            &rows[(bottom + 1).min(rows.len())..(bottom + 4).min(rows.len())]
        )
    })
}

/// A session model viewing one live chip — the owner's screenshot surface.
fn subagent_model() -> AppModel {
    let mut model = session_model();
    model.chips = vec![ChipModel::from_seed(ChipSeed {
        agent: "t1-docs".to_owned(),
        parent: None,
        ros: None,
        callsign: "Husayn".to_owned(),
        hon: "(r)",
        full: "Husayn ibn Ali".to_owned(),
        name: "docs".to_owned(),
        model: "fable-5".to_owned(),
        device: "macbook".to_owned(),
        state: ChipDisplayState::Running,
        tokens: 100,
        prefill: Vec::new(),
    })];
    model.screen = Screen::Subagent;
    model.view_path = vec!["t1-docs".to_owned()];
    model
}

#[test]
fn subagent_band_closes_with_a_rule_above_the_subtree() {
    // MUTATION CHECK (band-rule): delete the `if band_rule_h > 0` render
    // block from `render_subagent` and this fails — the owner's second
    // screenshot exactly (`❯ message Husayn…` straight into
    // `▼ subagents`). Verified by revert.
    let model = subagent_model();
    let (rows, _, terminal) = draw(&model, 100, 30);
    let below = assert_two_rules(&rows, "message Husayn", "message Husayn", "subagent");
    // The closing rule separates the band from the SubTree map — the
    // precise gap the owner's screenshot showed missing.
    let subtree_y = row_of(&rows, "subagents") as usize;
    assert!(
        below < subtree_y,
        "the closing rule sits BETWEEN the band and ▼ subagents"
    );
    // Frame ink, like every closing rule (sim border-top: frame).
    let theme = model.theme.theme();
    let buffer = terminal.backend().buffer();
    assert_eq!(
        buffer[(0, u16::try_from(below).expect("fits"))].fg,
        Color::from(theme.frame),
        "closing rule wears the frame ink"
    );
}

#[test]
fn subagent_question_card_band_closes_too() {
    // The question card REPLACES the chip composer inside the same band —
    // the anatomy holds on both forms.
    let mut model = subagent_model();
    let text = "Run the suite against testcontainers or mocks?";
    let options = ["testcontainers", "mocks"];
    model.chips[0].state = ChipDisplayState::InputRequired;
    model.chips[0].question = Some(ChipQuestion {
        recovery: false,
        text: text.to_owned(),
        options: options.iter().map(|o| (*o).to_owned()).collect(),
        resolved: false,
    });
    // The card renders from the chip transcript's open menu (the same
    // wiring the driver's ChipQuestion beat performs).
    model.chips[0]
        .transcript
        .apply(&EventPayload::MenuOpened(haider_protocol::menu::Menu {
            id: haider_protocol::ids::MenuId::new("t1-docs-q"),
            kind: haider_protocol::menu::MenuKind::Choice,
            title: text.to_owned(),
            body: vec![],
            options: options
                .iter()
                .enumerate()
                .map(|(index, label)| haider_protocol::menu::MenuOption {
                    key: format!("o{index}"),
                    label: (*label).to_owned(),
                    detail: None,
                    decision: None,
                })
                .collect(),
            blocking: false,
            scope: haider_protocol::menu::MenuScope::Subagent {
                agent: haider_protocol::ids::AgentId::new("t1-docs"),
            },
            origin: "subagent".to_owned(),
            ttl_ms: None,
            timeout_option: None,
        }));
    let (rows, _, _) = draw(&model, 100, 30);
    assert_two_rules(
        &rows,
        "Run the suite against",
        "↑↓ select",
        "subagent question card",
    );
}

#[test]
fn session_band_carries_both_rules() {
    let mut model = session_model();
    for c in "draft".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    let (rows, _, _) = draw(&model, 100, 30);
    assert_two_rules(&rows, "❯ draft", "❯ draft", "session");
}

#[test]
fn session_menu_band_carries_both_rules() {
    // A blocking menu replaces the composer (sim §3 law); the band's two
    // rules survive the swap — warn above, frame below.
    let mut model = AppModel::new();
    for payload in demo_script() {
        if matches!(payload, EventPayload::MenuAnswered(_)) {
            break;
        }
        model.handle(AppEvent::Envelope(Box::new(payload)));
    }
    let menu_title = model
        .projection
        .open_menu()
        .expect("demo menu open")
        .title
        .clone();
    let anchor: String = menu_title.chars().take(20).collect();
    let (rows, _, _) = draw(&model, 118, 34);
    assert_two_rules(&rows, &anchor, "↑↓ select", "session menu");
}

#[test]
fn launcher_band_carries_both_rules() {
    // TUI5 item 1b fixed this surface; the sweep pins it alongside its
    // siblings so the pair can never regress apart again.
    let model = launcher_model();
    let (rows, _, _) = draw(&model, 100, 30);
    assert_two_rules(&rows, "start a session", "start a session", "launcher");
}

#[test]
fn aura_band_carries_both_rules() {
    let mut model = launcher_model();
    for c in "/aura".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    assert_eq!(model.screen, Screen::Aura);
    let (rows, _, _) = draw(&model, 100, 30);
    assert_two_rules(&rows, "speak or type", "speak or type", "aura");
}

#[test]
fn arg_slot_band_carries_both_rules() {
    // The arg-slot is the SAME band with staged text + the palette above
    // it (palette_area sits above rule_area, so the gold rule still
    // closes the palette against the band's first row).
    let mut model = session_model();
    for c in "/theme ".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    assert!(model.palette_open());
    let (rows, _, _) = draw(&model, 118, 34);
    assert_two_rules(&rows, "❯ /theme", "❯ /theme", "arg-slot");
}

#[test]
fn login_card_band_carries_both_rules() {
    // The masked login card replaces the composer CONTENT inside the same
    // band — it inherits the hosting surface's two rules.
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    for c in "/login anthropic api".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Esc));
    model.handle(key(KeyCode::Enter));
    assert!(
        model.login.is_some(),
        "card open (flash: {:?})",
        model.flash
    );
    let (rows, _, _) = draw(&model, 100, 30);
    assert_two_rules(&rows, "API key", "esc cancel", "login card");
}

#[test]
fn closing_rule_outranks_breathing_rows_under_pressure() {
    // MUTATION CHECK (closing-rule-outranks-breathing): restore the
    // TUI6b claim order (leads before band_rule_h/band_pad in
    // render_session) and this fails at height 14 — a blank row with no
    // rule between the band and ▾ subagents. Verified by revert.
    //
    // TUI6 review MINOR 1: the session ledger claimed the breathing rows
    // BEFORE the closing band rule, so at exact starvation a blank
    // breathing row survived while the rule shed — the owner's item-6
    // defect recreated with a blank in the rule's place (and the local
    // comment always said breathing rows shed FIRST). The fixed claim
    // order makes this invariant hold at EVERY height: whenever any row
    // separates the band from the SubTree header, at least one of the
    // separating rows is a rule.
    let mut model = session_model();
    model.chips = vec![ChipModel::from_seed(ChipSeed {
        agent: "t1-docs".to_owned(),
        parent: None,
        ros: None,
        callsign: "Husayn".to_owned(),
        hon: "(r)",
        full: "Husayn ibn Ali".to_owned(),
        name: "docs".to_owned(),
        model: "fable-5".to_owned(),
        device: "macbook".to_owned(),
        state: ChipDisplayState::Running,
        tokens: 100,
        prefill: Vec::new(),
    })];
    for height in 8..32 {
        let (rows, _, _) = draw(&model, 90, height);
        let Some(band_y) = rows.iter().position(|row| row.contains("message haider")) else {
            continue;
        };
        let Some(subtree_y) = rows.iter().position(|row| row.contains("subagents —")) else {
            continue;
        };
        if subtree_y <= band_y + 1 {
            continue; // zero spare rows — an honest full shed
        }
        assert!(
            rows[band_y + 1..subtree_y].iter().any(|row| is_rule(row)),
            "height {height}: rows between the band and the SubTree hold \
             no rule — a breathing/pad row outlived the closing rule: {:?}",
            &rows[band_y..=subtree_y]
        );
    }
}

// ---- TUI6.1 fix 2: the reserved closing rule, height-swept ----

/// The sweep law (review r1 finding 2), one assertion for every surface:
/// at ANY height, if `optional` content renders, BOTH band rules render —
/// an optional row must never outlive the closing rule. `band` finds the
/// input band's row; heights where the band itself is shed are skipped
/// (nothing to close).
fn sweep_two_rules(
    model: &AppModel,
    width: u16,
    band_top: &str,
    band_bottom: &str,
    optional: &[&str],
    surface: &str,
) {
    for height in 1..=16 {
        let (rows, _, _) = draw(model, width, height);
        let optional_shown = optional
            .iter()
            .any(|needle| rows.iter().any(|row| row.contains(needle)));
        if !optional_shown {
            continue;
        }
        let top = match rows.iter().position(|row| row.contains(band_top)) {
            Some(top) => top,
            None => {
                // TUI6.2 fix 4 de-blind (review r2: this arm silently
                // `continue`d, so a title-less question card passed the
                // sweep unnoticed): a band bottom without its top is the
                // dignity regression itself, never a skip.
                assert!(
                    !rows.iter().any(|row| row.contains(band_bottom)),
                    "{surface} at {width}x{height}: the band's bottom renders \
                     without its top — options without their question: {rows:?}"
                );
                continue;
            }
        };
        let bottom = rows
            .iter()
            .rposition(|row| row.contains(band_bottom))
            .unwrap_or(top)
            .max(top);
        assert!(
            top > 0 && is_rule(&rows[top - 1]),
            "{surface} at {width}x{height}: optional content renders but the \
             TOP rule is missing: {rows:?}"
        );
        assert!(
            (1..=3).any(|d| bottom + d < rows.len() && is_rule(&rows[bottom + d])),
            "{surface} at {width}x{height}: optional content renders but the \
             CLOSING rule is missing: {rows:?}"
        );
    }
}

#[test]
fn reserved_rule_sweeps_launcher() {
    // MUTATION CHECK (band-rule-reserve law): make `band_rule_reserve`
    // return 0 and every surface breaks at once — this sweep's 90×4 pin,
    // the session/subagent/aura pins below, and the launcher/aura
    // ladders' debug_asserts (all five ledgers route through the ONE law
    // function). Verified by revert.
    let model = launcher_model();
    sweep_two_rules(
        &model,
        90,
        "start a session",
        "start a session",
        &["recent sessions"],
        "launcher",
    );
    // Reviewer point pin — launcher 90×4: the OPTIONAL content column
    // yields and the triple (top rule · composer · closing rule) renders.
    let (rows, _, _) = draw(&model, 90, 4);
    assert_two_rules(&rows, "start a session", "start a session", "launcher@90x4");
    assert!(
        !rows.iter().any(|row| row.contains("recent sessions")),
        "the content column yielded to the closing rule at 90×4"
    );
}

#[test]
fn reserved_rule_sweeps_session_with_chip() {
    let mut model = session_model();
    model.chips = vec![ChipModel::from_seed(ChipSeed {
        agent: "t1-docs".to_owned(),
        parent: None,
        ros: None,
        callsign: "Husayn".to_owned(),
        hon: "(r)",
        full: "Husayn ibn Ali".to_owned(),
        name: "docs".to_owned(),
        model: "fable-5".to_owned(),
        device: "macbook".to_owned(),
        state: ChipDisplayState::Running,
        tokens: 100,
        prefill: Vec::new(),
    })];
    sweep_two_rules(
        &model,
        90,
        "message haider",
        "message haider",
        &["subagents", "✳ Waiting"],
        "session+chip",
    );
    // Reviewer point pin — session with chip 90×11: the SubTree must not
    // outbid the closing rule.
    let (rows, _, _) = draw(&model, 90, 11);
    assert_two_rules(
        &rows,
        "message haider",
        "message haider",
        "session+chip@90x11",
    );
}

#[test]
fn reserved_rule_holds_on_the_session_menu() {
    // Reviewer point pin — session menu 90×10: a blank spacer row
    // survived where the closing rule fit; the reserve now takes the gap
    // row itself when the budget is dry.
    let mut model = AppModel::new();
    for payload in demo_script() {
        if matches!(payload, EventPayload::MenuAnswered(_)) {
            break;
        }
        model.handle(AppEvent::Envelope(Box::new(payload)));
    }
    assert!(model.projection.open_menu().is_some(), "demo menu open");
    // Band bottom = the LAST OPTION row ("2. Deny") — the footer hint is
    // the card's first shed under pressure, and the options are the
    // sacred floor the rule must close beneath.
    let (rows, _, _) = draw(&model, 90, 10);
    assert_two_rules(&rows, "Allow fs_patch", "2. Deny", "session-menu@90x10");
    // And across 8..=16 the same law holds wherever ANY row exists
    // between the last option and the status row — a lower row (pad,
    // gap, panel) must never outlive the closing rule. At the
    // exactly-full heights (e.g. 90×8: header + rules + the transcript's
    // sacred row + both options fill the frame to the brim) there is
    // nothing below the options and no rule is owed: the transcript's
    // sacred row OUTRANKS the closing rule (session parity), so the
    // triple no longer physically fits — the law's own shed case.
    for height in 8..=16 {
        let (rows, _, _) = draw(&model, 90, height);
        let Some(last_option) = rows.iter().rposition(|row| row.contains("2. Deny")) else {
            continue;
        };
        if last_option + 2 >= rows.len() {
            continue; // nothing between the options and the status row
        }
        assert!(
            (1..=3).any(|d| last_option + d < rows.len() && is_rule(&rows[last_option + d])),
            "menu band closes at 90×{height}: {rows:?}"
        );
    }
}

#[test]
fn reserved_rule_sweeps_subagent_and_question_card() {
    let model = subagent_model();
    sweep_two_rules(
        &model,
        90,
        "message Husayn",
        "message Husayn",
        &["subagents"],
        "subagent",
    );
    // Reviewer point pin — subagent composer 90×11.
    let (rows, _, _) = draw(&model, 90, 11);
    assert_two_rules(&rows, "message Husayn", "message Husayn", "subagent@90x11");
    // Question-card form, reviewer point pin — 90×14.
    let mut model = subagent_model();
    let text = "Run the suite against testcontainers or mocks?";
    let options = ["testcontainers", "mocks"];
    model.chips[0].state = ChipDisplayState::InputRequired;
    model.chips[0].question = Some(ChipQuestion {
        recovery: false,
        text: text.to_owned(),
        options: options.iter().map(|o| (*o).to_owned()).collect(),
        resolved: false,
    });
    model.chips[0]
        .transcript
        .apply(&EventPayload::MenuOpened(haider_protocol::menu::Menu {
            id: haider_protocol::ids::MenuId::new("t1-docs-q"),
            kind: haider_protocol::menu::MenuKind::Choice,
            title: text.to_owned(),
            body: vec![],
            options: options
                .iter()
                .enumerate()
                .map(|(index, label)| haider_protocol::menu::MenuOption {
                    key: format!("o{index}"),
                    label: (*label).to_owned(),
                    detail: None,
                    decision: None,
                })
                .collect(),
            blocking: false,
            scope: haider_protocol::menu::MenuScope::Subagent {
                agent: haider_protocol::ids::AgentId::new("t1-docs"),
            },
            origin: "subagent".to_owned(),
            ttl_ms: None,
            timeout_option: None,
        }));
    // Band top = the card TITLE (the warn rule sits above it); band
    // bottom = the last OPTION row (the footer is the card's first shed).
    sweep_two_rules(
        &model,
        90,
        "Run the suite against",
        "2. mocks",
        &["subagents"],
        "subagent-question",
    );
    // Bottom anchor = the last OPTION ("2. mocks") — the footer hint is
    // the card's first shed and is already gone at this height.
    let (rows, _, _) = draw(&model, 90, 14);
    assert_two_rules(&rows, "Run the suite against", "2. mocks", "question@90x14");
}

#[test]
fn question_card_never_sheds_its_title_while_options_render() {
    // Review r2 finding 4, the exact counterexample: a FOUR-option card
    // at 90×12 rendered options 1-4 with the closing rule and a blank
    // optional gap — and NO question. The card's floor is now
    // title + options (session parity), so wherever the options render,
    // the question renders above them.
    //
    // MUTATION CHECK (title-in-floor): revert the subagent ledger's
    // `floor_input` to `options.len()` (drop the `+ 1`) and this fails
    // at 90×12 — options without their question. Verified by revert.
    let mut model = subagent_model();
    let text = "Which environment should the suite target for this run?";
    let options = ["testcontainers", "mocks", "staging", "production"];
    model.chips[0].state = ChipDisplayState::InputRequired;
    model.chips[0].question = Some(ChipQuestion {
        recovery: false,
        text: text.to_owned(),
        options: options.iter().map(|o| (*o).to_owned()).collect(),
        resolved: false,
    });
    model.chips[0]
        .transcript
        .apply(&EventPayload::MenuOpened(haider_protocol::menu::Menu {
            id: haider_protocol::ids::MenuId::new("t1-docs-q4"),
            kind: haider_protocol::menu::MenuKind::Choice,
            title: text.to_owned(),
            body: vec![],
            options: options
                .iter()
                .enumerate()
                .map(|(index, label)| haider_protocol::menu::MenuOption {
                    key: format!("o{index}"),
                    label: (*label).to_owned(),
                    detail: None,
                    decision: None,
                })
                .collect(),
            blocking: false,
            scope: haider_protocol::menu::MenuScope::Subagent {
                agent: haider_protocol::ids::AgentId::new("t1-docs"),
            },
            origin: "subagent".to_owned(),
            ttl_ms: None,
            timeout_option: None,
        }));
    for height in 1..=20 {
        let (rows, _, _) = draw(&model, 90, height);
        let options_shown = rows.iter().any(|row| row.contains("4. production"));
        // The reviewer's law is a PRIORITY law, not a physics law: the
        // title must never shed while an OPTIONAL row survives (the r2
        // frame kept a blank gap and the SubTree). At heights where the
        // frame is exactly the sacred options (e.g. 90×4 = 4 option
        // rows, nothing else), the title's absence is the documented
        // physical degenerate — options outrank the title inside
        // menu_block, and there is no optional row to trade away.
        let optional_survives = rows.iter().any(|row| row.contains("subagents"))
            || rows
                .iter()
                .take(rows.len().saturating_sub(1))
                .any(|row| row.trim().is_empty());
        if options_shown && optional_survives {
            assert!(
                rows.iter().any(|row| row.contains("Which environment")),
                "90×{height}: options render without their question while \
                 optional rows survive: {rows:?}"
            );
        }
    }
    // The r2 frame itself: title + all four options + both rules.
    let (rows, _, _) = draw(&model, 90, 12);
    assert_two_rules(
        &rows,
        "Which environment",
        "4. production",
        "question4@90x12",
    );
}

#[test]
fn reserved_rule_sweeps_aura() {
    let mut model = launcher_model();
    for c in "/aura".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    assert_eq!(model.screen, Screen::Aura);
    sweep_two_rules(
        &model,
        90,
        "speak or type",
        "speak or type",
        &["hold to talk", "controlled sessions"],
        "aura",
    );
    // Reviewer point pin — aura 90×10: orb/columns must not outbid the
    // closing rule.
    let (rows, _, _) = draw(&model, 90, 10);
    assert_two_rules(&rows, "speak or type", "speak or type", "aura@90x10");
}

// ---- Item 7: the thinner header mark ----

#[test]
fn header_mark_is_two_thirds_thinner_and_keeps_its_anatomy() {
    use haider_tui::mark;
    /// Ink groups in one pixel row (a letterform stroke = one group).
    fn groups(row: &str) -> usize {
        row.split('.').filter(|s| !s.is_empty()).count()
    }
    // Two-thirds of the TUI4 24-col raster, exactly; the 28-col
    // launcher/boot banner is out of scope and untouched.
    assert_eq!(mark::HEADER_COLS, 16);
    assert_eq!(mark::HEADER_ROWS, 2);
    assert_eq!(mark::BANNER_COLS, 28, "launcher banner untouched");
    for (index, row) in mark::HEADER.iter().enumerate() {
        assert_eq!(row.len(), 16, "pixel row {index} is 16 wide");
    }
    // Anatomy (the same letters, narrower): the upright rows carry FOUR
    // ink groups — `ر` `ـد` `ـيـ` `حـ` in visual order…
    assert_eq!(groups(mark::HEADER[0]), 4);
    assert_eq!(groups(mark::HEADER[1]), 4);
    // …the baseline is ONE run reaching the right edge, with `ر` standing
    // clear of it (the word breaks at `د`, which does not join forward)…
    assert_eq!(groups(mark::HEADER[2]), 1);
    assert!(mark::HEADER[2].ends_with('#'));
    assert!(mark::HEADER[2].starts_with("...."), "ر stands clear");
    // …and the descender row carries THREE groups: the tail plus `ي`'s
    // two dots, still separated — the legibility floor is the dot GAP,
    // which the two-thirds rework preserves.
    assert_eq!(groups(mark::HEADER[3]), 3);
    // The rendering never exceeds the declared cells, and stays pure
    // half-block ink.
    for row in mark::header_rows() {
        assert!(row.trim_end().chars().count() <= 16);
        assert!(row.chars().all(|c| "█▀▄ ".contains(c)));
    }
}

#[test]
fn header_mark_dignity_gate_holds_at_the_new_threshold() {
    // MUTATION CHECK (mark dignity gate): make `header_fits` return true
    // unconditionally and this fails — a 53-col session would draw the
    // art into a header that cannot hold it instead of stepping down to
    // the text tier (whole or nothing; fall back rather than mangle).
    // Verified by revert.
    use haider_tui::mark;
    let threshold = mark::HEADER_COLS + 38; // render's HEADER_MARK_RESERVED
    assert!(mark::header_fits(threshold, 38));
    assert!(!mark::header_fits(threshold - 1, 38));
    let mut model = launcher_model();
    for c in "walk me through".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    assert_eq!(model.screen, Screen::Session);
    // At the threshold the art spans BOTH header lines…
    let (rows, _, _) = draw(&model, threshold, 30);
    let art = mark::header_rows();
    assert!(rows[0].contains(art[0].trim_end()), "art line 1");
    assert!(rows[1].contains(art[1].trim_end()), "art line 2");
    // …one cell under it, the text mark returns and no art row leaks.
    let (rows, _, _) = draw(&model, threshold - 1, 30);
    assert!(rows[0].contains("حيدر"), "text-mark tier below the gate");
    assert!(!rows[0].contains(art[0].trim_end()), "no clipped art");
}

// ---- Multi-line + wrap compose: logical lines wrap independently ----

#[test]
fn newlines_and_wrap_compose_into_one_visual_row_walk() {
    let mut model = session_model();
    for _ in 0..100 {
        model.handle(key(KeyCode::Char('a')));
    }
    model.handle(key_mod(KeyCode::Enter, KeyModifiers::ALT));
    for c in "tail".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    // 100 cells wrap into 2 rows at 90 cols (budget 85) + the "tail" line.
    let (_, hits, _) = draw(&model, 90, 34);
    let windows = composer_windows(&hits);
    assert_eq!(windows.len(), 3);
    assert_eq!(windows[2].2, "tail");
    // ↑ from "tail"'s end: onto the WRAP TAIL of the long line (visual
    // row 2), not its logical start — the visual-row walk.
    model.handle(key(KeyCode::Up));
    assert_eq!(model.composer.cursor(), 89, "col 4 of the wrap tail row");
    model.handle(key(KeyCode::Up));
    assert_eq!(model.composer.cursor(), 4, "col 4 of the first wrap row");
}
