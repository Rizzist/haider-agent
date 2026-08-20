//! Review-round-2 (NO_SHIP 46aff07) fix guards: the post-interrupt envelope
//! race, stale hit maps, idle(i) decay plumbing, the real multi-line
//! composer, raw-UTF-16 paste thresholds, menu body lines, cell-accurate
//! pre-wrap agent bodies, and the dim IDLE_I badge.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::ids::{ItemId, MenuId};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_tui::app::{AppEvent, AppModel, AppRequest, Hit, Screen};
use haider_tui::mock::demo_script;
use haider_tui::render::render;
use haider_tui::runtime::IDLE_DECAY;
use haider_tui::script::DemoEvent;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::Color;

mod common;
use common::{drain, driver_for, key, launcher_model};

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

// ---- P1-1 (+ r3 P3-7): post-interrupt envelope race, PRODUCTION wiring ----

#[tokio::test(start_paused = true)]
async fn stale_generation_envelopes_are_dropped_at_consumption() {
    // The real race through the real wiring (review r3 P3-7): the driver's
    // channel, its spawned script on virtual time, its bump, its guard.
    let mut model = launcher_model();
    let (mut driver, mut rx) = driver_for(&model);
    for c in "walk me through the harness".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    drain(&mut driver, &mut model);
    // Consume script beats until the turn's UserMessage attaches the view.
    while model.screen != Screen::Session {
        let (generation, payload) = rx.recv().await.expect("script beat");
        driver.consume(&mut model, generation, payload);
    }
    assert!(model.turn_active);

    // Pull the NEXT beat but hold it — this is the buffered envelope of
    // the race — then Esc lands FIRST and bumps the generation.
    let (stale_generation, stale_payload) = rx.recv().await.expect("buffered beat");
    model.handle(key(KeyCode::Esc));
    drain(&mut driver, &mut model);
    assert!(!model.turn_active);
    assert!(model.projection.interrupted());
    assert!(
        !driver.is_arm_live(stale_generation),
        "the interrupt cancelled that arm"
    );
    let entries_before = model.projection.entries().len();

    // The buffered stale beat is consumed AFTER the bump: dropped whole.
    driver.consume(&mut model, stale_generation, stale_payload);
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

    // On virtual time the guarded 30s decay arrives through the SAME
    // channel and lands (its generation is current).
    assert_eq!(IDLE_DECAY.as_secs(), 30, "sim decay window (tui.js:1562)");
    loop {
        let (generation, payload) = rx.recv().await.expect("decay beat");
        let is_decay = matches!(payload, DemoEvent::Envelope(EventPayload::IdleDecayed));
        driver.consume(&mut model, generation, payload);
        if is_decay {
            break;
        }
        assert!(model.projection.interrupted(), "stale beats change nothing");
    }
    assert!(!model.projection.interrupted(), "the 30s decay landed");
}

#[tokio::test(start_paused = true)]
async fn stale_idle_decay_never_lands_in_a_fresh_session() {
    // r3 P3-6: interrupt session A, start fresh session B within the 30s
    // window, interrupt B too — A's pending decay must be dropped (its
    // generation is stale); only B's OWN decay clears B's idle(i).
    let mut model = launcher_model();
    let (mut driver, mut rx) = driver_for(&model);
    for c in "walk me through the harness".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    drain(&mut driver, &mut model);
    while model.screen != Screen::Session {
        let (generation, payload) = rx.recv().await.expect("script beat");
        driver.consume(&mut model, generation, payload);
    }
    // Interrupt A (schedules A's decay).
    model.handle(key(KeyCode::Esc));
    drain(&mut driver, &mut model);
    assert!(model.projection.interrupted());

    // Fresh session B within the window (⌃C to launcher — esc is
    // session-scoped now (owner directive) — type, submit).
    model.handle(AppEvent::Key(ratatui::crossterm::event::KeyEvent::new(
        KeyCode::Char('c'),
        ratatui::crossterm::event::KeyModifiers::CONTROL,
    )));
    for c in "start something new".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    drain(&mut driver, &mut model);
    assert_eq!(model.screen, Screen::Session);
    // Interrupt B (schedules B's decay, another bump).
    model.handle(key(KeyCode::Esc));
    drain(&mut driver, &mut model);
    assert!(model.projection.interrupted(), "B is idle(i)");

    // Consume everything on virtual time. TUI4c (directed — the law got
    // STRONGER): A's decay is no longer a stale-generation drop; it ROUTES
    // to A's slot by session id (the sim's timeout writes `runStates[A]`
    // wherever A lives, tui.js:1561-1564). B's idle(i) is untouched by it
    // and only B's OWN decay clears B.
    let mut decays_seen = 0;
    while decays_seen < 2 {
        let (generation, payload) = rx.recv().await.expect("beat");
        let is_decay = matches!(payload, DemoEvent::Envelope(EventPayload::IdleDecayed));
        driver.consume(&mut model, generation, payload);
        if is_decay {
            decays_seen += 1;
            if decays_seen == 1 {
                assert!(
                    model.projection.interrupted(),
                    "A's decay left B's idle(i) alone"
                );
                let slot_a = model
                    .sessions
                    .iter()
                    .find(|entry| entry.name.as_deref() == Some("walk-me-through"))
                    .expect("A's slot");
                assert!(
                    !slot_a.projection.interrupted(),
                    "A's own idle(i) decayed IN ITS SLOT"
                );
            }
        }
    }
    assert!(!model.projection.interrupted(), "B's own decay cleared it");
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
    assert_eq!(
        model.screen,
        haider_tui::app::Screen::Tree,
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
    assert_ne!(
        model.screen,
        haider_tui::app::Screen::Tree,
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
    // TUI4c (directed): `new_session` cancels nothing — the previous
    // session keeps running in its slot; only the submit is requested.
    assert_eq!(
        model.requests,
        vec![AppRequest::SubmitText {
            text: "line one\nline two\nline three".to_owned(),
            voice: false,
            title: true,
            branch: None,
            attachments: vec![],
        }]
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
    // Directed (TUI5 item 1): the trailing `▮` glyph is retired — the
    // cursor row is found by its text; the cursor is asserted as a CELL.
    let third_y = row_of(&rows, "gamma");
    assert_eq!(second_y, first_y + 1);
    assert_eq!(third_y, first_y + 2, "one row per line");
    let buffer = terminal.backend().buffer();
    // The gold rule still sits directly above the first composer row and
    // every composer row keeps the input ground.
    assert_eq!(buffer[(0, first_y - 1)].fg, Color::from(theme.gold));
    for y in [first_y, second_y, third_y] {
        assert_eq!(buffer[(0, y)].bg, Color::from(theme.input_bg));
    }
    // TUI5 item 1: the cursor CELL sits after "gamma" — gutter(2+2) +
    // text(5) puts it at x=9 on the cursor row.
    assert_eq!(buffer[(9, third_y)].bg, Color::from(theme.gold));
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
    let (rows, _, terminal) = draw(&model, 118, 34);
    assert!(
        !rows.iter().any(|row| row.contains("❯ one")),
        "head scrolled"
    );
    let marker_y = row_of(&rows, "⋮ four");
    assert_eq!(row_of(&rows, "seven"), marker_y + 3, "tail rows visible");
    // Directed (TUI5 item 1): the last row is the trailing EMPTY line —
    // its cursor is now a styled cell (gold ground over a space) in the
    // text column (pad 2 + gutter 2), not a `▮` glyph to grep for.
    let theme = model.theme.theme();
    let buffer = terminal.backend().buffer();
    assert_eq!(
        buffer[(4, marker_y + 4)].bg,
        Color::from(theme.gold),
        "cursor cell on the fifth visible row"
    );
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
    // Directed (TUI6 item 1, re-scoping the TUI5 form): the overlong line
    // now WRAPS — the horizontal tail-window and its `…` marker are
    // outlawed in the composer. The caret stays visible by the same law,
    // as a styled cell right after the tail text on the LAST wrapped row,
    // with the wrapped head rows directly above.
    let (rows, _, terminal) = draw(&model, 90, 34);
    let composer_y = row_of(&rows, "TAIL");
    assert!(
        !rows[composer_y as usize].contains('…'),
        "no ellipsis in the composer (TUI6): {:?}",
        rows[composer_y as usize]
    );
    assert!(
        rows[(composer_y - 1) as usize].contains("xxx"),
        "the wrapped head row sits directly above the tail row"
    );
    let theme = model.theme.theme();
    let buffer = terminal.backend().buffer();
    let tail_x = col_of(&rows[composer_y as usize], "TAIL");
    assert_eq!(
        buffer[(tail_x + 4, composer_y)].bg,
        Color::from(theme.gold),
        "cursor cell right after the visible tail"
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
    // Directed (TUI5 item 1): "c▮" became the styled cursor cell after
    // "c" — the row is located by geometry, the caret by cell style.
    let (rows, _, terminal) = draw(&model, 90, 10);
    let first_y = row_of(&rows, "❯ a");
    assert!(
        rows[(first_y + 2) as usize].starts_with("    c"),
        "composer rows intact"
    );
    let theme = model.theme.theme();
    let buffer = terminal.backend().buffer();
    assert_eq!(
        buffer[(5, first_y + 2)].bg,
        Color::from(theme.gold),
        "cursor cell after the third row's text"
    );
}

// ---- P2-6 / r3 P2-2: render is the single scroll authority ----

#[test]
fn wheel_before_first_frame_and_resize_never_bank_debt() {
    use haider_tui::runtime::dispatch_input;
    use ratatui::crossterm::event::Event;
    let mut model = launcher_model();
    model.handle(AppEvent::Envelope(Box::new(user_message("hello"))));
    assert_eq!(model.screen, Screen::Session);
    // Reconcile-then-apply (review r5 P2-2): pre-frame wheel intent clamps
    // to the last known truth (0) immediately — no invisible debt, ever.
    model.handle_wheel(true);
    assert_eq!(model.scroll_back.get(), 0, "no invisible pre-frame debt");
    // Overflowing frame, scroll to the top — a burst stops exactly at the
    // known top with no redraw needed…
    let mut model = session_model();
    let (_, _, _) = draw(&model, 90, 14);
    for _ in 0..50 {
        model.handle_wheel(true);
    }
    let tall_max = model.scroll_max.get();
    assert_eq!(model.scroll_back.get(), tall_max);
    // …then ENLARGE, resize arriving through the production input path
    // (review r3 P3-7): the event only dirties — the NEXT FRAME reconciles
    // the offset against the new smaller range (render is the authority).
    model.dirty = false;
    dispatch_input(&mut model, &[], Event::Resize(90, 30));
    assert!(model.dirty, "resize forces a redraw");
    let (_, _, _) = draw(&model, 90, 30);
    let short_max = model.scroll_max.get();
    assert!(short_max < tall_max);
    assert_eq!(
        model.scroll_back.get(),
        short_max,
        "the frame repaid the debt — no invisible remainder"
    );
    // Shrinking back does NOT resurrect the old offset.
    dispatch_input(&mut model, &[], Event::Resize(90, 14));
    let (_, _, _) = draw(&model, 90, 14);
    assert_eq!(model.scroll_back.get(), short_max, "no debt resurrection");
}

#[test]
fn fresh_session_resets_the_scroll_ceiling() {
    // A scrolled session's ceiling must not leak into a fresh session
    // (review r3 P2-2).
    let mut model = session_model();
    let (_, _, _) = draw(&model, 90, 14);
    for _ in 0..50 {
        model.handle_wheel(true);
    }
    assert!(model.scroll_back.get() > 0);
    for c in "/clear".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    assert_eq!(model.screen, Screen::Launcher);
    assert_eq!(model.scroll_back.get(), 0, "fresh session starts at tail");
    assert_eq!(model.scroll_max.get(), 0, "old ceiling gone");
    // The next session's first frame rebuilds its own truth.
    model.handle(AppEvent::Envelope(Box::new(user_message("fresh start"))));
    let (_, _, _) = draw(&model, 90, 14);
    assert_eq!(model.scroll_back.get(), 0, "new session at its tail");
}

// ---- P2-7: raw UTF-16 paste thresholds ----

#[test]
fn paste_thresholds_measure_raw_utf16_units() {
    // 151 emoji = 302 UTF-16 units (> 300) on ONE line → the pill (the
    // QoL wave's Claude Code placeholder; the thresholds are unchanged).
    let mut model = launcher_model();
    model.handle(AppEvent::Paste("🌊".repeat(151).into()));
    assert_eq!(model.composer, "[Pasted text #1 +1 lines]");
    // Exactly 300 ASCII units on one line → literal (not > 300).
    let mut model = launcher_model();
    model.handle(AppEvent::Paste("x".repeat(300).into()));
    assert_eq!(model.composer, "x".repeat(300));
    // Raw newline count beats normalization: 4 CRLF lines → the pill.
    let mut model = launcher_model();
    model.handle(AppEvent::Paste("a\r\nb\r\nc\r\nd".to_owned().into()));
    assert_eq!(model.composer, "[Pasted text #1 +4 lines]");
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
    let body_y = row_of(&rows, "fs_edit wants to modify");
    let body_x = col_of(&rows[body_y as usize], "fs_edit wants");
    let cell = &buffer[(body_x, body_y)];
    assert_eq!(cell.fg, Color::from(theme.dim), "body line dim");
    assert_eq!(cell.bg, Color::from(theme.gold_soft), "menu ground");
    // Both body lines sit between title and first option.
    let title_y = row_of(&rows, "? Allow fs_edit — event_store.rs?");
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
    model.handle(AppEvent::Envelope(Box::new(user_message(
        "fix the failing boundary test",
    ))));
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
