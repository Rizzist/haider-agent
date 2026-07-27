//! TUI5 — first-class composer cursor (owner wave off v0.0.10).
//!
//! Items 1-3 + 1b guards: the cursor as a STYLED CELL (never an appended
//! glyph), grapheme-aware movement/editing at the cursor, the launcher
//! band's closing rule, and the composer model's edge cases (combining
//! marks, wide CJK, Arabic logical order, kills, sticky columns,
//! selection ops, the input ring).
#![allow(clippy::expect_used)]

use haider_tui::app::{AppEvent, AppModel, AppRequest, Hit, LauncherRow, Screen};
use haider_tui::composer::Composer;
use haider_tui::mock::demo_script;
use haider_tui::render::render;
use haider_tui::runtime::dispatch_input;
use haider_tui::theme::ThemeKey;
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
    let mut rows = Vec::new();
    for y in 0..buffer.area.height {
        let mut line = String::new();
        for x in 0..buffer.area.width {
            line.push_str(buffer[(x, y)].symbol());
        }
        rows.push(line);
    }
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

// ---- Item 3: editing at the cursor (model level) ----

#[test]
fn typing_inserts_at_the_cursor_never_appends() {
    // MUTATION CHECK (insert-at-cursor): revert Composer::insert_str to
    // push_str (append) and this fails — the owner's core complaint.
    let mut c = Composer::new();
    c.insert_str("helo");
    c.move_left(false);
    c.insert_str("l");
    assert_eq!(c.text(), "hello");
    assert_eq!(c.cursor(), 4, "cursor rides just past the insert");
    // Mid-word multi-char insert.
    c.line_home(false);
    c.insert_str(">> ");
    assert_eq!(c.text(), ">> hello");
}

#[test]
fn backspace_and_delete_are_grapheme_aware() {
    let mut c = Composer::new();
    // e + combining acute travels as ONE grapheme.
    c.insert_str("caf\u{65}\u{301}!");
    c.move_left(false); // before '!'
    c.backspace(); // removes the whole é cluster
    assert_eq!(c.text(), "caf!");
    // Wide CJK: one grapheme, two cells.
    let mut c = Composer::new();
    c.insert_str("a漢b");
    c.line_home(false);
    c.move_right(false);
    c.delete_forward(); // removes 漢 after the cursor
    assert_eq!(c.text(), "ab");
    // Empty buffer: every editing op is a safe no-op.
    let mut c = Composer::new();
    c.backspace();
    c.delete_forward();
    c.word_backspace();
    c.kill_to_line_end();
    c.kill_to_line_start();
    assert_eq!(c.text(), "");
    assert_eq!(c.cursor(), 0);
}

#[test]
fn arabic_moves_in_logical_order() {
    // The session placeholder's Arabic-adjacent reality: logical-order
    // movement means ← ALWAYS walks toward the string start (item 2's
    // documented choice; bidi caret geometry is ledgered graphics-tier).
    let mut c = Composer::new();
    c.insert_str("حيدر");
    assert_eq!(c.cursor(), "حيدر".len());
    c.move_left(false);
    assert_eq!(c.cursor(), "حيد".len(), "one letter toward the start");
    c.line_home(false);
    c.move_right(false);
    assert_eq!(c.cursor(), "ح".len(), "one letter toward the end");
}

#[test]
fn word_movement_and_word_backspace() {
    let mut c = Composer::new();
    c.insert_str("alpha beta  gamma");
    c.word_left(false);
    assert_eq!(c.cursor(), 12, "start of gamma");
    c.word_left(false);
    assert_eq!(c.cursor(), 6, "start of beta");
    c.word_right(false);
    assert_eq!(c.cursor(), 10, "end of beta");
    c.line_end_key(false);
    c.word_backspace();
    assert_eq!(c.text(), "alpha beta  ");
    c.word_backspace();
    assert_eq!(c.text(), "alpha ", "kills the word AND its trailing gap");
}

#[test]
fn kill_commands_are_line_scoped() {
    let mut c = Composer::new();
    c.insert_str("one\ntwo three\nfour");
    // Put the cursor after "two" on line 2.
    c.line_up(false);
    c.line_home(false);
    c.word_right(false);
    assert_eq!(c.cursor(), 7);
    c.kill_to_line_end();
    assert_eq!(c.text(), "one\ntwo\nfour", "⌃K eats to the line end");
    // ⌃K AT the line end kills the newline (Emacs C-k law).
    c.kill_to_line_end();
    assert_eq!(c.text(), "one\ntwofour");
    // ⌃U kills to the line start; at the line start it is a no-op
    // (readline unix-line-discard).
    c.kill_to_line_start();
    assert_eq!(c.text(), "one\nfour");
    c.kill_to_line_start();
    assert_eq!(c.text(), "one\nfour", "⌃U at line start does nothing");
}

// ---- Item 2: vertical movement, column-sticky ----

#[test]
fn vertical_movement_is_column_sticky() {
    let mut c = Composer::new();
    c.insert_str("longest line\nab\nmiddle line");
    // Cursor at the end of "middle line" (col 11).
    assert!(c.line_up(false), "row 3 → row 2");
    assert_eq!(c.cursor(), 15, "clamped to the END of the short 'ab' row");
    assert!(c.line_up(false), "row 2 → row 1");
    // Sticky column: back at col 11 on the long first row, NOT col 2.
    assert_eq!(c.cursor(), 11);
    assert!(!c.line_up(false), "already on the first row");
    assert!(c.line_down(false));
    assert!(c.line_down(false));
    assert_eq!(c.cursor(), "longest line\nab\nmiddle line".len());
    assert!(!c.line_down(false), "already on the last row");
}

#[test]
fn vertical_movement_counts_wide_cells() {
    let mut c = Composer::new();
    // 漢漢 = 4 display cells; the target col falling INSIDE a wide glyph
    // keeps the cursor before it.
    c.insert_str("漢漢漢\nabc");
    assert_eq!(c.cursor(), "漢漢漢\nabc".len());
    assert!(c.line_up(false)); // col 3 lands inside the second 漢
    assert_eq!(c.cursor(), "漢".len(), "snapped before the straddled glyph");
}

// ---- Item 4 groundwork: the selection model (keys land in slice 2) ----

#[test]
fn shift_movement_extends_and_plain_movement_collapses() {
    let mut c = Composer::new();
    c.insert_str("hello world");
    c.move_left(true);
    c.move_left(true);
    assert_eq!(c.selection_range(), Some((9, 11)));
    assert_eq!(c.selected_text(), Some("ld"));
    // Plain ← collapses to the selection's LEFT edge without stepping
    // (native-input law).
    c.move_left(false);
    assert!(!c.has_selection());
    assert_eq!(c.cursor(), 9);
    // Plain → after a fresh selection collapses to the RIGHT edge.
    c.move_left(true);
    c.move_right(false);
    assert_eq!(c.cursor(), 9);
    assert!(!c.has_selection());
}

#[test]
fn typing_replaces_the_selection() {
    // MUTATION CHECK (selection-replace-on-type): drop the
    // delete_selection_if_any call from insert_str and this fails.
    let mut c = Composer::new();
    c.insert_str("hello world");
    c.line_home(false);
    c.word_right(true);
    assert_eq!(c.selected_text(), Some("hello"));
    c.insert_str("goodbye");
    assert_eq!(c.text(), "goodbye world");
    assert!(!c.has_selection());
    // ⌫ deletes the selection alone.
    c.line_home(false);
    c.word_right(true);
    c.backspace();
    assert_eq!(c.text(), " world");
}

// ---- Item 6: the input ring (model level) ----

#[test]
fn history_ring_recalls_and_restores_the_draft() {
    let mut c = Composer::new();
    c.insert_str("first");
    assert_eq!(c.take_for_submit(), "first");
    c.insert_str("second");
    assert_eq!(c.take_for_submit(), "second");
    // A live draft survives the browse round-trip.
    c.insert_str("draft");
    assert!(c.history_prev());
    assert_eq!(c.text(), "second");
    assert_eq!(c.cursor(), c.text().len(), "recall puts the cursor at END");
    assert!(c.history_prev());
    assert_eq!(c.text(), "first");
    assert!(!c.history_prev(), "at the oldest entry");
    assert!(c.history_next());
    assert!(c.history_next());
    assert_eq!(c.text(), "draft", "past the newest entry the draft returns");
    // Consecutive duplicates dedupe.
    c.clear();
    c.insert_str("same");
    let _ = c.take_for_submit();
    c.insert_str("same");
    let _ = c.take_for_submit();
    assert!(c.history_prev());
    assert_eq!(c.text(), "same");
    assert!(c.history_prev(), "…straight past the single 'same' entry");
    assert_eq!(c.text(), "second");
}

// ---- Item 5 groundwork: mouse byte snapping ----

#[test]
fn press_snaps_to_grapheme_boundaries() {
    let mut c = Composer::new();
    c.insert_str("a\u{65}\u{301}z"); // a, é (e + combining acute), z
    // A byte INSIDE the é cluster is not a caret stop — snap to its start.
    c.press_at(2);
    assert_eq!(c.cursor(), 1);
    // Past-the-end bytes clamp to the end.
    c.press_at(999);
    assert_eq!(c.cursor(), c.text().len());
    // Press parks the anchor; a drag grows the selection from it.
    c.press_at(1);
    assert!(!c.has_selection(), "zero-width press is no selection");
    c.drag_to(4);
    assert_eq!(c.selected_text(), Some("\u{65}\u{301}"));
}

// ---- Item 1: the cursor CELL on screen ----

#[test]
fn cursor_renders_reverse_video_mid_text() {
    let mut model = session_model();
    for c in "hello".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Left));
    model.handle(key(KeyCode::Left));
    let theme = model.theme.theme();
    let (rows, _, terminal) = draw(&model, 118, 34);
    let buffer = terminal.backend().buffer();
    let y = row_of(&rows, "❯ hello");
    assert!(
        !rows[y as usize].contains('▮'),
        "no appended glyph anywhere"
    );
    // pad(2) + sigil(2) + "hel" → the cursor cell carries the SECOND 'l'.
    let cell = &buffer[(7, y)];
    assert_eq!(cell.symbol(), "l", "the glyph under the caret stays");
    assert_eq!(cell.bg, Color::from(theme.gold), "reverse-video ground");
    assert_eq!(cell.fg, Color::from(theme.badge_fg), "reverse-video ink");
    // Typing here inserts BETWEEN the halves (item 3, end to end).
    model.handle(key(KeyCode::Char('X')));
    assert_eq!(model.composer, "helXlo");
}

#[test]
fn cursor_cell_is_themed_on_every_theme() {
    for key_name in ["dawn", "ivory", "dark"] {
        let mut model = launcher_model();
        let theme_key = ThemeKey::parse(key_name).expect("theme name");
        model.theme = theme_key;
        let theme = theme_key.theme();
        // Empty composer: the block sits BEFORE the dim placeholder
        // (Claude Code's treatment), on the launcher too (item 1b).
        let (rows, _, terminal) = draw(&model, 100, 30);
        let buffer = terminal.backend().buffer();
        let y = row_of(&rows, "start a session");
        let cell = &buffer[(4, y)];
        assert_eq!(cell.symbol(), " ", "{key_name}: block over a space");
        assert_eq!(cell.bg, Color::from(theme.gold), "{key_name}: gold ground");
        assert_eq!(cell.fg, Color::from(theme.badge_fg), "{key_name}: bg ink");
    }
}

#[test]
fn launcher_band_closes_with_a_frame_rule() {
    // Item 1b: the owner's "no line under it" report — the launcher band
    // now closes with the frame rule the sim's StatusBar border-top draws.
    let model = launcher_model();
    let theme = model.theme.theme();
    let (rows, _, terminal) = draw(&model, 100, 30);
    let buffer = terminal.backend().buffer();
    let composer_y = row_of(&rows, "start a session");
    let closing = &rows[composer_y as usize + 1];
    assert!(
        closing.chars().filter(|c| *c == '─').count() >= 90,
        "closing rule under the launcher band, got {closing:?}"
    );
    assert_eq!(
        buffer[(0, composer_y + 1)].fg,
        Color::from(theme.frame),
        "the closing rule wears the frame ink (sim StatusBar border-top)"
    );
    // The rule ABOVE stays gold — the band is closed on both edges.
    assert_eq!(buffer[(0, composer_y - 1)].fg, Color::from(theme.gold));
}

#[test]
fn cursor_row_follows_the_caret_through_the_vertical_window() {
    let mut model = session_model();
    for (index, line) in ["one", "two", "three", "four", "five", "six"]
        .iter()
        .enumerate()
    {
        if index > 0 {
            model.handle(key_mod(KeyCode::Enter, KeyModifiers::ALT));
        }
        for c in line.chars() {
            model.handle(key(KeyCode::Char(c)));
        }
    }
    // Tail preferred: rows two..six visible, head scrolled (⋮ above).
    let (rows, _, _) = draw(&model, 118, 34);
    assert!(rows.iter().any(|row| row.contains("⋮ two")), "head hidden");
    assert!(!rows.iter().any(|row| row.contains("❯ one")));
    // Walk the caret to the FIRST row: the window follows it up and the
    // hidden tail is marked below.
    for _ in 0..5 {
        model.handle(key(KeyCode::Up));
    }
    let (rows, _, _) = draw(&model, 118, 34);
    let first_y = row_of(&rows, "❯ one");
    assert!(
        rows[(first_y + 4) as usize].contains("⋮ five"),
        "hidden-below marker on the window's last row: {:?}",
        &rows[(first_y + 4) as usize]
    );
    assert!(!rows.iter().any(|row| row.contains("six")), "tail hidden");
}

#[test]
fn overlong_line_windows_around_a_mid_text_caret() {
    let mut model = session_model();
    for _ in 0..200 {
        model.handle(key(KeyCode::Char('x')));
    }
    // Caret to the very start: the head shows, the tail clips with ….
    model.handle(key(KeyCode::Home));
    let theme = model.theme.theme();
    let (rows, _, terminal) = draw(&model, 90, 34);
    let buffer = terminal.backend().buffer();
    let y = row_of(&rows, "❯ xxx");
    let row = &rows[y as usize];
    assert!(
        row.ends_with('…') || row.trim_end().ends_with('…'),
        "right clip: {row:?}"
    );
    assert!(!row.contains("… x"), "no left clip at the head");
    // The caret cell is the FIRST x, reverse-video.
    let cell = &buffer[(4, y)];
    assert_eq!(cell.symbol(), "x");
    assert_eq!(cell.bg, Color::from(theme.gold));
    // Typing at the head INSERTS there.
    model.handle(key(KeyCode::Char('A')));
    assert!(model.composer.text().starts_with("Ax"));
}

#[test]
fn aura_and_arg_slot_composers_carry_the_cursor_cell() {
    // Arg-slot state: "/theme " with the palette open — the caret cell
    // sits after the trailing space, the ghost after IT.
    let mut model = session_model();
    for c in "/theme ".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    assert!(model.palette_open());
    let theme = model.theme.theme();
    let (rows, _, terminal) = draw(&model, 118, 34);
    let buffer = terminal.backend().buffer();
    let y = row_of(&rows, "❯ /theme");
    // pad(2)+sigil(2)+"/theme "(7) → caret cell at x=11.
    assert_eq!(buffer[(11, y)].bg, Color::from(theme.gold));
    // Aura's own composer instance (item 1's state list).
    let mut model = launcher_model();
    for c in "/aura".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    assert_eq!(model.screen, Screen::Aura);
    for c in "hi".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    let (rows, _, terminal) = draw(&model, 118, 34);
    let buffer = terminal.backend().buffer();
    let y = row_of(&rows, "❯ hi");
    assert_eq!(
        buffer[(6, y)].bg,
        Color::from(theme.gold),
        "aura composer caret cell"
    );
}

// ---- Item 4: the selection keys ----

#[test]
fn ctrl_c_with_selection_copies_instead_of_navigating() {
    // MUTATION CHECK (⌃C selection-vs-navigation gate): drop the
    // selection_key call from handle() and the FIRST ⌃C below navigates
    // to the launcher (and the launcher half quits) — both asserts fail.
    let mut model = session_model();
    for c in "secret".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    for _ in 0..3 {
        model.handle(key_mod(KeyCode::Left, KeyModifiers::SHIFT));
    }
    assert_eq!(model.composer.selected_text(), Some("ret"));
    model.handle(key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert_eq!(model.screen, Screen::Session, "⌃C with selection stays put");
    assert!(
        model
            .requests
            .contains(&AppRequest::CopyText("ret".to_owned())),
        "the selection copied: {:?}",
        model.requests
    );
    assert!(!model.composer.has_selection(), "copy clears the selection");
    // The SECOND ⌃C has no selection — the TUI4 navigation law fires
    // unchanged.
    model.handle(key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert_eq!(model.screen, Screen::Launcher, "bare ⌃C navigates");
    // Launcher half of the gate: a selection blocks the quit too.
    for c in "draft".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key_mod(KeyCode::Left, KeyModifiers::SHIFT));
    model.handle(key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(!model.should_quit, "⌃C with selection never quits");
    model.handle(key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(model.should_quit, "bare ⌃C on the launcher quits");
}

#[test]
fn ctrl_c_with_transcript_selection_recopies_the_frame() {
    let mut model = session_model();
    model.selection = Some(haider_tui::select::Selection {
        anchor: (0, 2),
        head: (10, 2),
        dragging: false,
    });
    model.handle(key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert_eq!(model.screen, Screen::Session, "no navigation");
    assert!(model.requests.contains(&AppRequest::CopySelection));
    assert!(
        model.selection.is_some(),
        "the highlight survives the copy (mouse-up parity)"
    );
}

#[test]
fn esc_clears_the_selection_before_any_other_esc_meaning() {
    let mut model = session_model();
    model.turn_active = true;
    for c in "hi".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key_mod(KeyCode::Left, KeyModifiers::SHIFT));
    assert!(model.composer.has_selection());
    model.handle(key(KeyCode::Esc));
    assert!(!model.composer.has_selection(), "esc deselects first");
    assert!(model.turn_active, "…and does NOT interrupt on that press");
    assert!(!model.requests.contains(&AppRequest::Interrupt));
    model.handle(key(KeyCode::Esc));
    assert!(!model.turn_active, "the NEXT esc interrupts as before");
    assert!(model.requests.contains(&AppRequest::Interrupt));
}

#[test]
fn selection_band_renders_with_a_distinct_cursor_cell() {
    let mut model = session_model();
    for c in "hello".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    // ⇧← twice: selection over "lo", caret at its LEFT (active) end.
    model.handle(key_mod(KeyCode::Left, KeyModifiers::SHIFT));
    model.handle(key_mod(KeyCode::Left, KeyModifiers::SHIFT));
    let theme = model.theme.theme();
    let (rows, _, terminal) = draw(&model, 118, 34);
    let buffer = terminal.backend().buffer();
    let y = row_of(&rows, "❯ hello");
    // pad(2)+sigil(2)+"hel" → 'l' at x=7 wears the CURSOR cell (the
    // active end stays distinct, item 4)…
    assert_eq!(buffer[(7, y)].bg, Color::from(theme.gold));
    // …and 'o' at x=8 wears the selection band.
    assert_eq!(buffer[(8, y)].bg, Color::from(theme.sel_bg));
    assert_eq!(buffer[(8, y)].fg, Color::from(theme.bright));
    // Unselected cells keep the input ground.
    assert_eq!(buffer[(5, y)].bg, Color::from(theme.input_bg));
}

// ---- Item 5: mouse in the composer ----

fn mouse(kind: MouseEventKind, column: u16, row: u16) -> Event {
    Event::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    })
}

fn composer_text_rect(hits: &[(Rect, Hit)]) -> (Rect, usize, String) {
    hits.iter()
        .find_map(|(rect, hit)| match hit {
            Hit::ComposerText { start, content } => Some((*rect, *start, content.clone())),
            _ => None,
        })
        .expect("a composer text region in the hit map")
}

#[test]
fn click_places_the_caret_at_the_clicked_grapheme() {
    let mut model = session_model();
    for c in "hello world".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    let (_, hits, _) = draw(&model, 118, 34);
    let (rect, start, _) = composer_text_rect(&hits);
    assert_eq!(start, 0, "single unclipped row starts at byte 0");
    dispatch_input(
        &mut model,
        &hits,
        mouse(MouseEventKind::Down(MouseButton::Left), rect.x + 3, rect.y),
    );
    assert_eq!(model.composer.cursor(), 3, "caret at the clicked column");
    assert!(model.composer_drag, "press arms the composer drag mode");
    dispatch_input(
        &mut model,
        &hits,
        mouse(MouseEventKind::Up(MouseButton::Left), rect.x + 3, rect.y),
    );
    assert!(!model.composer_drag);
    assert!(
        !model
            .requests
            .iter()
            .any(|r| matches!(r, AppRequest::CopyText(_))),
        "a plain click copies nothing"
    );
    // Clicking PAST the text end parks the caret at the line end.
    dispatch_input(
        &mut model,
        &hits,
        mouse(MouseEventKind::Down(MouseButton::Left), rect.x + 60, rect.y),
    );
    assert_eq!(model.composer.cursor(), "hello world".len());
    dispatch_input(
        &mut model,
        &hits,
        mouse(MouseEventKind::Up(MouseButton::Left), rect.x + 60, rect.y),
    );
}

#[test]
fn composer_drag_selects_and_autocopies_on_release() {
    let mut model = session_model();
    for c in "hello world".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    let (_, hits, _) = draw(&model, 118, 34);
    let (rect, _, _) = composer_text_rect(&hits);
    dispatch_input(
        &mut model,
        &hits,
        mouse(MouseEventKind::Down(MouseButton::Left), rect.x, rect.y),
    );
    dispatch_input(
        &mut model,
        &hits,
        mouse(MouseEventKind::Drag(MouseButton::Left), rect.x + 5, rect.y),
    );
    assert_eq!(model.composer.selected_text(), Some("hello"));
    assert!(
        model.selection.is_none(),
        "a drag STARTING in the composer is never a screen selection \
         (region disambiguation, item 5)"
    );
    dispatch_input(
        &mut model,
        &hits,
        mouse(MouseEventKind::Up(MouseButton::Left), rect.x + 5, rect.y),
    );
    assert!(
        model
            .requests
            .contains(&AppRequest::CopyText("hello".to_owned())),
        "auto-copy on release, transcript parity: {:?}",
        model.requests
    );
    assert!(
        model.composer.has_selection(),
        "the highlight survives the release"
    );
    // Dragging BELOW the band clamps to the text end (native law).
    model.requests.clear();
    dispatch_input(
        &mut model,
        &hits,
        mouse(MouseEventKind::Down(MouseButton::Left), rect.x + 6, rect.y),
    );
    dispatch_input(
        &mut model,
        &hits,
        mouse(MouseEventKind::Drag(MouseButton::Left), rect.x, rect.y + 3),
    );
    assert_eq!(model.composer.selected_text(), Some("world"));
    dispatch_input(
        &mut model,
        &hits,
        mouse(MouseEventKind::Up(MouseButton::Left), rect.x, rect.y + 3),
    );
}

#[test]
fn transcript_drag_still_makes_a_screen_selection() {
    let mut model = session_model();
    let (_, hits, _) = draw(&model, 118, 34);
    let (rect, _, _) = composer_text_rect(&hits);
    // Start WELL above the composer band, on transcript rows.
    let y = rect.y.saturating_sub(10);
    dispatch_input(
        &mut model,
        &hits,
        mouse(MouseEventKind::Down(MouseButton::Left), 5, y),
    );
    assert!(
        !model.composer_drag,
        "transcript press never arms the composer"
    );
    dispatch_input(
        &mut model,
        &hits,
        mouse(MouseEventKind::Drag(MouseButton::Left), 30, y + 1),
    );
    assert!(
        model.selection.is_some(),
        "screen-space selection as in TUI4b"
    );
    assert!(!model.composer.has_selection());
    dispatch_input(
        &mut model,
        &hits,
        mouse(MouseEventKind::Up(MouseButton::Left), 30, y + 1),
    );
    assert!(model.requests.contains(&AppRequest::CopySelection));
}

#[test]
fn stale_composer_hit_is_dropped_not_misapplied() {
    let mut model = session_model();
    for c in "ab".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    // A hit whose window starts beyond the CURRENT text (stale frame after
    // a kill) must drop the press whole — never place a phantom caret.
    model.composer_press(50, "stale content", 3);
    assert_eq!(model.composer.cursor(), 2, "cursor untouched");
    assert!(!model.composer_drag, "no drag armed from a dropped press");
}

// ---- Item 6: history interplay through the arrow keys ----

#[test]
fn arrow_history_recalls_submits_and_restores_the_draft() {
    let mut model = session_model();
    for text in ["alpha task", "beta task"] {
        for c in text.chars() {
            model.handle(key(KeyCode::Char(c)));
        }
        model.handle(key(KeyCode::Enter));
    }
    assert!(model.composer.is_empty());
    for c in "draft".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    // ↑ on the FIRST (only) row recalls the previous submit, cursor at
    // END (Claude Code behavior).
    model.handle(key(KeyCode::Up));
    assert_eq!(model.composer, "beta task");
    assert_eq!(model.composer.cursor(), "beta task".len());
    model.handle(key(KeyCode::Up));
    assert_eq!(model.composer, "alpha task");
    // ↓ walks forward; past the newest entry the DRAFT returns.
    model.handle(key(KeyCode::Down));
    assert_eq!(model.composer, "beta task");
    model.handle(key(KeyCode::Down));
    assert_eq!(model.composer, "draft");
}

#[test]
fn up_moves_rows_first_and_recalls_only_at_the_edge() {
    let mut model = session_model();
    for c in "seed".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    // A two-row draft: ↑ from the last row MOVES first (item 6: history
    // only from the first visual row).
    for c in "one".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key_mod(KeyCode::Enter, KeyModifiers::ALT));
    for c in "two".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Up));
    assert_eq!(model.composer, "one\ntwo", "row move, no recall");
    assert!(model.composer.on_first_line());
    model.handle(key(KeyCode::Up));
    assert_eq!(model.composer, "seed", "the edge row recalls");
    // ⇧↑ at the edge is a SELECTION gesture, never a recall (item 4).
    model.handle(key(KeyCode::Down));
    for c in "x".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    let text_now = model.composer.text().to_owned();
    model.handle(key_mod(KeyCode::Up, KeyModifiers::SHIFT));
    assert_eq!(model.composer, text_now.as_str(), "⇧↑ never recalls");
}

#[test]
fn palette_owns_the_arrows_while_open() {
    let mut model = session_model();
    for c in "seed".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    for c in "/t".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    assert!(model.palette_open());
    let selection_before = model.palette_selection;
    model.handle(key(KeyCode::Down));
    assert_ne!(model.palette_selection, selection_before, "palette moved");
    assert_eq!(
        model.composer, "/t",
        "no history recall through the palette"
    );
}

// ---- Item 9: per-surface drafts ----

#[test]
fn drafts_travel_per_surface_with_cursor_and_selection() {
    let mut model = launcher_model();
    for c in "launcher draft".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    let sid = model.sessions[0].id;
    model.open_session(sid);
    assert!(model.composer.is_empty(), "a fresh session starts fresh");
    for c in "session one".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key_mod(KeyCode::Left, KeyModifiers::SHIFT));
    assert_eq!(model.composer.selected_text(), Some("e"));
    // Esc first clears the selection (item 4)… so detach via the direct
    // navigation law instead, selection intact.
    model.back_to_launcher();
    assert_eq!(model.composer, "launcher draft", "launcher draft restored");
    model.open_session(sid);
    assert_eq!(model.composer, "session one", "session draft restored");
    assert_eq!(
        model.composer.selected_text(),
        Some("e"),
        "cursor AND selection travelled with the draft"
    );
    // A second session has its own (empty) draft.
    let sid2 = model.sessions[1].id;
    model.back_to_launcher();
    model.open_session(sid2);
    assert!(model.composer.is_empty());
    // Aura keeps its own instance — enter via the launcher's Aura row
    // (the value-carrying hit), which must NOT eat the launcher draft.
    model.back_to_launcher();
    assert_eq!(model.composer, "launcher draft");
    model.handle_hit(Hit::ExtraRow(LauncherRow::Aura));
    assert_eq!(model.screen, Screen::Aura);
    assert!(model.composer.is_empty(), "aura's own draft, fresh");
    for c in "aura draft".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Esc));
    assert_eq!(model.screen, Screen::Launcher);
    assert_eq!(
        model.composer, "launcher draft",
        "launcher draft back after the aura visit"
    );
    model.handle_hit(Hit::ExtraRow(LauncherRow::Aura));
    assert_eq!(model.composer, "aura draft", "aura draft survived the exit");
}

#[test]
fn submit_clears_only_that_surfaces_draft() {
    let mut model = launcher_model();
    for c in "parked".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    let sid = model.sessions[0].id;
    model.open_session(sid);
    for c in "send me".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    assert!(
        model.composer.is_empty(),
        "submit cleared the session draft"
    );
    model.back_to_launcher();
    assert_eq!(model.composer, "parked", "the launcher draft is untouched");
}

#[test]
fn reset_purges_session_drafts_but_the_launcher_draft_survives() {
    use haider_tui::app::DraftKey;
    let mut model = launcher_model();
    let sid2 = model.sessions[1].id;
    model.open_session(sid2);
    for c in "doomed".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.back_to_launcher();
    assert!(model.drafts.contains_key(&DraftKey::Session(sid2)));
    for c in "keep me".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    let sid1 = model.sessions[0].id;
    model.open_session(sid1);
    for c in "/reset".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    assert_eq!(model.screen, Screen::Launcher);
    assert_eq!(
        model.composer, "keep me",
        "the launcher draft survives /reset (documented choice: the \
         launcher is not an identity-keyed surface)"
    );
    assert!(
        !model
            .drafts
            .keys()
            .any(|key| matches!(key, DraftKey::Session(_) | DraftKey::Aura)),
        "session (and aura) drafts die with the reseed: {:?}",
        model.drafts.keys().collect::<Vec<_>>()
    );
}

// ---- Item 8: the persistence DTO must not grow ----

#[test]
fn dto_carries_no_composer_cursor_or_draft_state() {
    // MUTATION CHECK: add a composer/draft/cursor field to any DTO (or
    // serialize the live composer into snapshot()) and the key sweep
    // below fails. This is the item-8/9 assertion: drafts are transient
    // BY LAW, not by accident.
    let mut model = launcher_model();
    for c in "live draft".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key_mod(KeyCode::Left, KeyModifiers::SHIFT));
    let sid = model.sessions[0].id;
    model.open_session(sid);
    for c in "parked draft".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    let value =
        serde_json::to_value(haider_tui::demo_store::snapshot(&model)).expect("snapshot json");
    fn sweep(value: &serde_json::Value, bad: &[&str]) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, inner) in map {
                    let lower = key.to_ascii_lowercase();
                    assert!(
                        !bad.iter().any(|b| lower.contains(b)),
                        "persisted key {key:?} leaks composer state"
                    );
                    sweep(inner, bad);
                }
            }
            serde_json::Value::Array(items) => {
                for inner in items {
                    sweep(inner, bad);
                }
            }
            _ => {}
        }
    }
    sweep(
        &value,
        &[
            "composer",
            "draft",
            "cursor",
            "anchor",
            "selection",
            "history",
            "sticky",
        ],
    );
    // And the round-trip: hydrating a fresh model from this snapshot
    // leaves composer and drafts EMPTY — no stale cursor state can ride
    // a restart (item 8's identity law).
    let dto: haider_tui::demo_store::StateDto =
        serde_json::from_value(value).expect("dto round-trip");
    let mut fresh = AppModel::new();
    let _ = haider_tui::demo_store::hydrate(&mut fresh, dto);
    assert!(fresh.composer.is_empty());
    assert!(fresh.drafts.is_empty());
}

// ---- TUI4 carried P3-1 + P3-3 (folded here per the brief) ----

#[test]
fn load_rejects_sentinel_max_and_duplicate_ids() {
    use haider_tui::demo_store::{DemoStore, snapshot};
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("state.json");
    let model = launcher_model();
    let write = |dto: &haider_tui::demo_store::StateDto| {
        std::fs::write(&path, serde_json::to_string(dto).expect("json")).expect("write");
    };
    let store = DemoStore::at(path.clone());
    // A valid snapshot loads.
    let good = snapshot(&model);
    write(&good);
    assert!(store.load().is_some(), "baseline snapshot loads");
    // P3-1: id u64::MAX would overflow the guard-2 bump — reject whole.
    let mut bad = good.clone();
    bad.sessions[0].id = u64::MAX;
    write(&bad);
    assert!(store.load().is_none(), "id u64::MAX rejects to seeds");
    // …and the matching card_seq bound.
    let mut bad = good.clone();
    bad.card_seq = u64::MAX;
    write(&bad);
    assert!(store.load().is_none(), "card_seq u64::MAX rejects to seeds");
    // P3-3: duplicate session ids mirror-corrupt the next save — reject.
    let mut bad = good.clone();
    let first = bad.sessions[0].id;
    bad.sessions[1].id = first;
    write(&bad);
    assert!(store.load().is_none(), "duplicate ids reject to seeds");
}
