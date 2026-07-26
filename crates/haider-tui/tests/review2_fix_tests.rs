//! Review-round-2 (NO_SHIP 46aff07) fix guards: the post-interrupt envelope
//! race, stale hit maps, idle(i) decay plumbing, the real multi-line
//! composer, raw-UTF-16 paste thresholds, menu body lines, cell-accurate
//! pre-wrap agent bodies, and the dim IDLE_I badge.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::ids::{ItemId, MenuId};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::state::HarnessStatus;
use haider_tui::app::{AppEvent, AppModel, AppRequest, Hit, Screen};
use haider_tui::mock::demo_script;
use haider_tui::render::render;
use haider_tui::runtime::{IDLE_DECAY, consume_scripted};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::Color;

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

fn key(code: KeyCode) -> AppEvent {
    AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn key_mod(code: KeyCode, modifiers: KeyModifiers) -> AppEvent {
    AppEvent::Key(KeyEvent::new(code, modifiers))
}

fn row_of(rows: &[String], needle: &str) -> u16 {
    u16::try_from(
        rows.iter()
            .position(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("row containing {needle:?} not rendered")),
    )
    .expect("row fits u16")
}

fn col_of(row: &str, needle: &str) -> u16 {
    let byte = row
        .find(needle)
        .unwrap_or_else(|| panic!("column of {needle:?} not found in row {row:?}"));
    u16::try_from(row[..byte].chars().count()).expect("col fits u16")
}

fn launcher_model() -> AppModel {
    let mut model = AppModel::new();
    model.handle(AppEvent::Envelope(Box::new(EventPayload::HarnessStatus(
        HarnessStatus::Ready,
    ))));
    model
}

fn session_model() -> AppModel {
    let mut model = AppModel::new();
    for payload in demo_script() {
        model.handle(AppEvent::Envelope(Box::new(payload)));
    }
    model
}

fn user_message(text: &str) -> EventPayload {
    EventPayload::UserMessage {
        text: text.to_owned(),
        attachments: vec![],
        mode: haider_protocol::DeliveryMode::Steer,
    }
}

// ---- P1-1: post-interrupt envelope race ----

#[test]
fn stale_generation_envelopes_are_dropped_at_consumption() {
    // The exact race: a turn's envelope is already BUFFERED (tagged gen 0)
    // when Esc interrupts and bumps the generation to 1. The buffered
    // payload must be dropped at consumption — not replayed into the model.
    let mut model = launcher_model();
    model.handle(key(KeyCode::Char('1')));
    model.requests.clear();
    consume_scripted(
        &mut model,
        0,
        0,
        user_message("fix the failing boundary test"),
    );
    assert_eq!(model.screen, Screen::Session);
    assert!(model.turn_active);

    // Esc → interrupt; the runtime bumps the generation (0 → 1).
    model.handle(key(KeyCode::Esc));
    assert!(!model.turn_active);
    assert!(model.projection.interrupted());
    assert_eq!(model.requests, vec![AppRequest::Interrupt]);
    let entries_before = model.projection.entries().len();

    // The stale buffered UserMessage (gen 0) arrives AFTER the bump.
    consume_scripted(&mut model, 0, 1, user_message("stale buffered message"));
    assert!(
        !model.turn_active,
        "stale envelope must not re-arm the turn"
    );
    assert!(model.projection.interrupted(), "idle(i) intact");
    assert_eq!(
        model.projection.entries().len(),
        entries_before,
        "stale envelope leaves no transcript trace"
    );

    // A current-generation envelope still flows.
    consume_scripted(&mut model, 1, 1, EventPayload::IdleDecayed);
    assert!(!model.projection.interrupted(), "fresh decay applied");
}

#[test]
fn idle_decay_is_generation_guarded_and_thirty_seconds() {
    assert_eq!(IDLE_DECAY.as_secs(), 30, "sim decay window (tui.js:1562)");
    let mut model = launcher_model();
    model.handle(key(KeyCode::Char('1')));
    model.requests.clear();
    consume_scripted(
        &mut model,
        0,
        0,
        user_message("fix the failing boundary test"),
    );
    model.handle(key(KeyCode::Esc));
    assert!(model.projection.interrupted());
    // A decay scheduled by an OLDER interrupt (gen 1) is stale once a newer
    // interrupt bumped to 2 — dropped whole.
    consume_scripted(&mut model, 1, 2, EventPayload::IdleDecayed);
    assert!(model.projection.interrupted(), "stale decay dropped");
    // The decay of the CURRENT interrupt applies.
    consume_scripted(&mut model, 2, 2, EventPayload::IdleDecayed);
    assert!(!model.projection.interrupted(), "current decay lands");
}

// ---- P2-2: stale hit maps ----

#[test]
fn stale_hits_activate_the_carried_value_or_drop() {
    // Render the /t palette, then mutate the model BEFORE the next frame.
    let mut model = session_model();
    for c in "/t".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    let (_, hits, _) = draw(&model, 118, 34);
    let tree_hit = hits
        .iter()
        .find_map(|(_, h)| match h {
            Hit::PaletteRow(item) if item.label() == "/tree" => Some(h.clone()),
            _ => None,
        })
        .expect("tree row hit");

    // Backspace to "/" — the row INDEXES all shift, but the click still
    // activates exactly the value that was on screen.
    model.handle(key(KeyCode::Backspace));
    model.handle_hit(tree_hit.clone());
    assert!(
        model.flash.as_deref().unwrap_or("").contains("/tree"),
        "the clicked VALUE ran, not whatever drifted under its index"
    );

    // Dismissed palette: the same stale hit is dropped whole.
    let mut model = session_model();
    for c in "/t".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Esc));
    assert!(!model.palette_open());
    model.handle_hit(tree_hit.clone());
    assert!(
        !model.flash.as_deref().unwrap_or("").contains("/tree"),
        "click through a dismissed palette is dropped"
    );

    // Help overlay covers everything: all stale hits are inert.
    let mut model = session_model();
    model.help_open = true;
    model.handle_hit(tree_hit);
    assert!(model.flash.is_none(), "hits under the overlay are dropped");
    assert!(model.help_open);
}

#[test]
fn menu_hits_answer_only_their_own_menu() {
    let mut model = AppModel::new();
    for payload in demo_script() {
        if matches!(payload, EventPayload::MenuAnswered(_)) {
            break;
        }
        model.handle(AppEvent::Envelope(Box::new(payload)));
    }
    assert!(model.projection.open_menu().is_some());
    // A hit rendered for a DIFFERENT menu id never answers this one.
    model.handle_hit(Hit::MenuOption {
        menu: MenuId::new("some-other-menu"),
        index: 0,
    });
    assert!(model.outbox.is_empty(), "foreign menu hit dropped");
    // The matching id answers.
    model.handle_hit(Hit::MenuOption {
        menu: MenuId::new("t0-menu-1"),
        index: 1,
    });
    let answer = model.outbox.pop().expect("answer produced");
    assert_eq!(answer.option_key.as_deref(), Some("deny"));
}

// ---- P2-4: multi-line composer ----

#[test]
fn alt_and_shift_enter_insert_newlines_and_enter_submits() {
    let mut model = launcher_model();
    for c in "line one".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key_mod(KeyCode::Enter, KeyModifiers::ALT));
    for c in "line two".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key_mod(KeyCode::Enter, KeyModifiers::SHIFT));
    for c in "line three".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    assert_eq!(model.composer, "line one\nline two\nline three");
    model.handle(key(KeyCode::Enter));
    assert_eq!(model.composer, "", "plain ⏎ still submits");
    assert_eq!(
        model.requests,
        vec![AppRequest::SubmitText(
            "line one\nline two\nline three".to_owned()
        )]
    );
}

#[test]
fn composer_grows_rows_and_shows_multiline_text() {
    let mut model = session_model();
    for c in "alpha".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key_mod(KeyCode::Enter, KeyModifiers::ALT));
    for c in "beta".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key_mod(KeyCode::Enter, KeyModifiers::ALT));
    for c in "gamma".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    let theme = model.theme.theme();
    let (rows, _, terminal) = draw(&model, 118, 34);
    let first_y = row_of(&rows, "❯ alpha");
    let second_y = row_of(&rows, "beta");
    let third_y = row_of(&rows, "gamma▮");
    assert_eq!(second_y, first_y + 1);
    assert_eq!(third_y, first_y + 2, "one row per line");
    let buffer = terminal.backend().buffer();
    // The gold rule still sits directly above the first composer row and
    // every composer row keeps the input ground.
    assert_eq!(buffer[(0, first_y - 1)].fg, Color::from(theme.gold));
    for y in [first_y, second_y, third_y] {
        assert_eq!(buffer[(0, y)].bg, Color::from(theme.input_bg));
    }
}

#[test]
fn composer_caps_at_five_rows_showing_the_tail() {
    let mut model = session_model();
    for line in ["one", "two", "three", "four", "five", "six", "seven"] {
        for c in line.chars() {
            model.handle(key(KeyCode::Char(c)));
        }
        model.handle(key_mod(KeyCode::Enter, KeyModifiers::ALT));
    }
    // Composer now holds 8 lines (trailing empty); only the LAST five rows
    // render, with a ⋮ marker in the scrolled gutter.
    let (rows, _, _) = draw(&model, 118, 34);
    assert!(
        !rows.iter().any(|row| row.contains("❯ one")),
        "head scrolled"
    );
    let marker_y = row_of(&rows, "⋮ four");
    let last_y = row_of(&rows, "▮");
    assert_eq!(last_y - marker_y, 4, "five visible composer rows");
}

#[test]
fn overlong_composer_line_keeps_the_cursor_visible() {
    let mut model = session_model();
    for _ in 0..200 {
        model.handle(key(KeyCode::Char('x')));
    }
    for c in "TAIL".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    let (rows, _, _) = draw(&model, 90, 34);
    let composer_y = row_of(&rows, "TAIL▮");
    assert!(
        rows[composer_y as usize].contains('…'),
        "horizontal tail-window marker present"
    );
}

#[test]
fn newline_closes_the_palette_and_multiline_survives_the_transcript() {
    let mut model = session_model();
    for c in "/th".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    assert!(model.palette_open());
    model.handle(key_mod(KeyCode::Enter, KeyModifiers::ALT));
    assert!(
        !model.palette_open(),
        "a newline closes the palette (sim getSuggestions bails on \\n)"
    );
    // Multi-line user text renders pre-wrap in the transcript.
    let mut model = session_model();
    model.handle(AppEvent::Envelope(Box::new(user_message(
        "first line\nsecond line",
    ))));
    let (rows, _, _) = draw(&model, 118, 34);
    let first_y = row_of(&rows, "❯ first line");
    let second_y = row_of(&rows, "   second line");
    assert_eq!(second_y, first_y + 1, "newline kept in the user row");
}

#[test]
fn tiny_frame_keeps_a_three_line_composer_visible() {
    let mut model = session_model();
    for c in "a".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key_mod(KeyCode::Enter, KeyModifiers::ALT));
    for c in "b".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key_mod(KeyCode::Enter, KeyModifiers::ALT));
    for c in "c".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    // Input-sacred at 90×10: all three composer rows stay on screen.
    let (rows, _, _) = draw(&model, 90, 10);
    let first_y = row_of(&rows, "❯ a");
    assert_eq!(row_of(&rows, "c▮"), first_y + 2, "composer rows intact");
}

// ---- P2-6: first-frame wheel + resize clamp ----

#[test]
fn wheel_before_first_frame_and_resize_never_bank_debt() {
    let mut model = launcher_model();
    model.handle(AppEvent::Envelope(Box::new(user_message("hello"))));
    assert_eq!(model.screen, Screen::Session);
    // No frame rendered yet: scroll_max starts 0 — wheel-up is inert.
    model.handle_wheel(true);
    assert_eq!(model.scroll_back, 0, "no invisible pre-frame debt");
    // Overflowing frame, scroll up, then shrink-resize: the clamp re-runs.
    let mut model = session_model();
    let (_, _, _) = draw(&model, 90, 14);
    for _ in 0..50 {
        model.handle_wheel(true);
    }
    let tall_max = model.scroll_max.get();
    assert_eq!(model.scroll_back, tall_max);
    // A taller frame shrinks the range; render writes the smaller max…
    let (_, _, _) = draw(&model, 90, 30);
    let short_max = model.scroll_max.get();
    assert!(short_max < tall_max);
    // …and the resize event re-clamps the banked debt.
    model.handle_resize();
    assert_eq!(model.scroll_back, short_max, "resize re-clamps");
}

// ---- P2-7: raw UTF-16 paste thresholds ----

#[test]
fn paste_thresholds_measure_raw_utf16_units() {
    // 151 emoji = 302 UTF-16 units (> 300) on ONE line → tokenized.
    let mut model = launcher_model();
    model.handle(AppEvent::Paste("🌊".repeat(151)));
    assert_eq!(model.composer, "[Pasted 1 lines] ");
    // Exactly 300 ASCII units on one line → literal (not > 300).
    let mut model = launcher_model();
    model.handle(AppEvent::Paste("x".repeat(300)));
    assert_eq!(model.composer, "x".repeat(300));
    // Raw newline count beats normalization: 4 CRLF lines → tokenized.
    let mut model = launcher_model();
    model.handle(AppEvent::Paste("a\r\nb\r\nc\r\nd".to_owned()));
    assert_eq!(model.composer, "[Pasted 4 lines] ");
}

// ---- P2-8: menu body lines ----

#[test]
fn menu_body_renders_dim_on_the_menu_ground() {
    let mut model = AppModel::new();
    for payload in demo_script() {
        if matches!(payload, EventPayload::MenuAnswered(_)) {
            break;
        }
        model.handle(AppEvent::Envelope(Box::new(payload)));
    }
    let theme = model.theme.theme();
    let (rows, _, terminal) = draw(&model, 118, 34);
    let buffer = terminal.backend().buffer();
    let body_y = row_of(&rows, "fs_patch wants to modify");
    let body_x = col_of(&rows[body_y as usize], "fs_patch wants");
    let cell = &buffer[(body_x, body_y)];
    assert_eq!(cell.fg, Color::from(theme.dim), "body line dim");
    assert_eq!(cell.bg, Color::from(theme.gold_soft), "menu ground");
    // Both body lines sit between title and first option.
    let title_y = row_of(&rows, "? Allow fs_patch — event_store.rs?");
    let option_y = row_of(&rows, "1. Allow once");
    let second_body_y = row_of(&rows, "effect class: workspace write");
    assert!(title_y < body_y && body_y < second_body_y && second_body_y < option_y);
}

// ---- P2-10: cell-accurate pre-wrap agent bodies ----

#[test]
fn agent_body_wraps_by_cells_with_rail_on_every_row() {
    let mut model = session_model();
    let body = "中文宽字符测试中文宽字符测试中文宽字符测试中文宽字符测试中文宽字符测试中文宽字符测试\n\n  indented bullet stays indented\nsee https://example.com/an/extremely/long/unbreakable/path/that/cannot/fit/on/one/terminal/row/at/all/ever for details";
    model.handle(AppEvent::Envelope(Box::new(EventPayload::Item(
        ItemEvent::Completed {
            item_id: ItemId::new("wide-msg"),
            item: TurnItem::AgentMessage {
                text: body.to_owned(),
            },
        },
    ))));
    let width: u16 = 60;
    let (rows, _, _) = draw(&model, width, 40);
    // Every body row (CJK continuations included) carries the rail and
    // stays inside the frame: nothing renders in the last column beyond
    // the budget (rail rows never overflow into Paragraph re-wrapping,
    // which would drop the rail).
    let rail_rows: Vec<&String> = rows.iter().filter(|row| row.contains('▏')).collect();
    assert!(
        rail_rows.len() >= 6,
        "CJK + URL + indented lines all wrapped behind the rail: {}",
        rail_rows.len()
    );
    // The CJK text hard-wraps across MULTIPLE rail rows (width 2 per char;
    // TestBackend interleaves the skipped half-cells, so match one char).
    let cjk_rows = rows
        .iter()
        .filter(|row| row.contains('▏') && row.contains('中'))
        .count();
    assert!(
        cjk_rows >= 2,
        "double-width text wraps by cells: {cjk_rows}"
    );
    // The unbreakable URL hard-splits at the cell boundary onto rail rows.
    let url_rows = rows
        .iter()
        .filter(|row| {
            row.contains('▏') && (row.contains("example.com") || row.contains("unbreakable"))
        })
        .count();
    assert!(url_rows >= 1, "long URL split behind the rail");
    // Explicit blank line survives as a bare rail row.
    assert!(
        rows.iter()
            .any(|row| row.trim_end().ends_with('▏') && row.contains('▏')),
        "blank pre-wrap line keeps its rail row"
    );
    // Leading indentation preserved after the rail.
    assert!(rows.iter().any(|row| row.contains("▏   indented bullet")));
}

// ---- P2-11: IDLE_I badge is dim ----

#[test]
fn interrupted_idle_badge_is_dim_like_plain_idle() {
    let mut model = launcher_model();
    model.handle(key(KeyCode::Char('1')));
    model.requests.clear();
    consume_scripted(
        &mut model,
        0,
        0,
        user_message("fix the failing boundary test"),
    );
    model.handle(key(KeyCode::Esc));
    assert!(model.projection.interrupted());
    let theme = model.theme.theme();
    let (rows, _, terminal) = draw(&model, 118, 34);
    let status_y = u16::try_from(rows.len() - 1).expect("status row");
    let badge_x = col_of(&rows[status_y as usize], "⏸ IDLE (i)");
    assert_eq!(
        terminal.backend().buffer()[(badge_x, status_y)].fg,
        Color::from(theme.dim),
        "IDLE_I falls through to the dim outline (sim tui.js:5531-5547)"
    );
}
