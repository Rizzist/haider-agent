//! Review-round-4 fix guards: menu options outrank the transcript's sacred
//! row (90×7), the streaming cursor wraps WITH the body so the rail never
//! drops, and a wheel notch between resize and redraw is honored by the
//! next frame.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::ids::ItemId;
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_tui::app::{AppEvent, AppModel, Hit};
use haider_tui::mock::demo_script;
use haider_tui::render::render;
use haider_tui::runtime::dispatch_input;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::Event;
use ratatui::layout::Rect;

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

fn session_model() -> AppModel {
    let mut model = AppModel::new();
    for payload in demo_script() {
        model.handle(AppEvent::Envelope(Box::new(payload)));
    }
    model
}

fn menu_model() -> AppModel {
    let mut model = AppModel::new();
    for payload in demo_script() {
        if matches!(payload, EventPayload::MenuAnswered(_)) {
            break;
        }
        model.handle(AppEvent::Envelope(Box::new(payload)));
    }
    model
}

/// A STREAMING agent message (Started, no Completed).
fn streaming_message(id: &str, text: &str) -> EventPayload {
    EventPayload::Item(ItemEvent::Started {
        item_id: ItemId::new(id),
        item: TurnItem::AgentMessage {
            text: text.to_owned(),
        },
    })
}

// ---- P2-1: menu options outrank the transcript's sacred row ----

#[test]
fn menu_options_survive_ninety_by_seven() {
    let mut model = menu_model();
    let (rows, hits, _) = draw(&model, 90, 7);
    // Both options render — the transcript's sacred row yielded.
    let allow_y = row_of(&rows, "1. Allow once");
    let deny_y = row_of(&rows, "2. Deny");
    assert_eq!(deny_y, allow_y + 1);
    // Both are clickable, from RENDERED positions.
    for (index, y) in [(0usize, allow_y), (1usize, deny_y)] {
        assert!(
            hits.iter().any(|(rect, h)| {
                matches!(h, Hit::MenuOption { index: i, .. } if *i == index) && rect.y == y
            }),
            "option {index} clickable at 90×7"
        );
    }
    let deny_hit = hits
        .iter()
        .find_map(|(_, h)| match h {
            Hit::MenuOption { index: 1, .. } => Some(h.clone()),
            _ => None,
        })
        .expect("deny hit");
    model.handle_hit(deny_hit);
    let answer = model.outbox.pop().expect("answer produced");
    assert_eq!(answer.option_key.as_deref(), Some("deny"));
    // The ordinary short layout is untouched: 90×8 still shows both
    // options AND keeps a transcript row.
    let (rows, _, _) = draw(&model, 90, 8);
    let _ = row_of(&rows, "1. Allow once");
    let _ = row_of(&rows, "2. Deny");
}

// ---- P2-2: streaming cursor wraps with the body ----

#[test]
fn streaming_cursor_never_leaves_the_rail() {
    // Reviewer's repro: width 5 (content budget 2), streaming "ab" — the
    // cursor must hard-split onto its own RAILED row, not overflow.
    let mut model = session_model();
    model.handle(AppEvent::Envelope(Box::new(streaming_message(
        "stream-ab",
        "ab",
    ))));
    let (rows, _, _) = draw(&model, 5, 40);
    let cursor_y = row_of(&rows, "▮");
    assert!(
        rows[cursor_y as usize].contains('▏'),
        "cursor row carries the rail: {:?}",
        rows[cursor_y as usize]
    );
    // Ordinary width, last row EXACTLY fills the budget (23 − 3 = 20
    // cells): the accounted cursor lands alone on the next railed row.
    let mut model = session_model();
    model.handle(AppEvent::Envelope(Box::new(streaming_message(
        "stream-full",
        &"a".repeat(20),
    ))));
    let (rows, _, _) = draw(&model, 23, 40);
    let full_y = row_of(&rows, &"a".repeat(20));
    let cursor_y = row_of(&rows, "▮");
    assert_eq!(cursor_y, full_y + 1, "cursor pushed to its own row");
    assert!(
        rows[cursor_y as usize].contains('▏'),
        "that row is railed too"
    );
    assert!(
        !rows[full_y as usize].contains('▮'),
        "the exactly-full row holds no overflow"
    );
    // A last row with room keeps the cursor inline, gold-split intact.
    let mut model = session_model();
    model.handle(AppEvent::Envelope(Box::new(streaming_message(
        "stream-inline",
        "short",
    ))));
    let (rows, _, _) = draw(&model, 90, 40);
    let inline_y = row_of(&rows, "short▮");
    assert!(rows[inline_y as usize].contains('▏'));
}

// ---- P3-3: a wheel notch between resize and redraw is honored ----

#[test]
fn wheel_notch_between_resize_and_redraw_is_honored() {
    let mut model = session_model();
    // At 90×16 the transcript overflows a little; ride to the top.
    let (_, _, _) = draw(&model, 90, 16);
    for _ in 0..50 {
        model.handle_wheel(true);
    }
    let (_, _, _) = draw(&model, 90, 16);
    let old_max = model.scroll_max.get();
    assert!(old_max > 0, "overflow at 90×16");
    assert_eq!(model.scroll_back.get(), old_max, "at the old top");
    // SHRINK (range grows), then one wheel-up notch BEFORE any redraw:
    // raw intent is recorded, not clamped against the stale max.
    dispatch_input(&mut model, &[], Event::Resize(90, 12));
    model.handle_wheel(true);
    // The next frame reconciles against the TRUE (larger) range — the
    // notch is honored, not lost.
    let (_, _, _) = draw(&model, 90, 12);
    let new_max = model.scroll_max.get();
    assert!(new_max >= old_max + 3, "shrinking grew the range");
    assert_eq!(
        model.scroll_back.get(),
        old_max + 3,
        "the pre-redraw notch moved the view"
    );
}
