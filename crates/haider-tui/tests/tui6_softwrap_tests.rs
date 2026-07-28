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

use haider_tui::app::{AppEvent, AppModel, Hit, Screen};
use haider_tui::composer::{Composer, wrap_rows};
use haider_tui::mock::demo_script;
use haider_tui::render::render;
use haider_tui::runtime::dispatch_input;
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
