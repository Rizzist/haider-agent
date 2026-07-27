//! Review-round-6 fix guards: the composer inherits the chrome-shed ladder
//! (menu-close at 90×5 restores an editable composer), and the hit-map
//! seam guard admits only real, in-frame regions — no phantom targets at
//! any size.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_tui::app::{AppEvent, AppModel, Hit};
use haider_tui::mock::demo_script;
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::Rect;

mod common;
use common::key;

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

/// Every hit rect must be non-empty and fully inside the frame.
fn assert_hits_in_frame(hits: &[(Rect, Hit)], width: u16, height: u16) {
    for (rect, hit) in hits {
        assert!(rect.width > 0 && rect.height > 0, "empty hit for {hit:?}");
        assert!(
            rect.x + rect.width <= width && rect.y + rect.height <= height,
            "out-of-frame hit for {hit:?}: {rect:?} in {width}×{height}"
        );
    }
}

// ---- P2-1: the menu-close transition at 90×5 ----

#[test]
fn answering_the_menu_at_ninety_by_five_restores_an_editable_composer() {
    let mut model = menu_model();
    // Answer by digit — then the echo envelope closes the card, exactly
    // the production seam.
    model.handle(key(KeyCode::Char('1')));
    let answer = model.outbox.pop().expect("answer produced").answer;
    model.handle(AppEvent::Envelope(Box::new(EventPayload::MenuAnswered(
        answer,
    ))));
    assert!(model.projection.open_menu().is_none(), "card closed");
    let (rows, hits, _) = draw(&model, 90, 5);
    // The composer is BACK — sacred through the same chrome-shed ladder
    // the menu used (the status row yielded at this height).
    let composer_y = rows
        .iter()
        .position(|row| row.contains('❯'))
        .expect("composer row visible after the menu closes");
    assert!(
        !rows.iter().any(|row| row.contains("IDLE")),
        "status row shed for the sacred composer"
    );
    assert_hits_in_frame(&hits, 90, 5);
    // Any talk chip sits ON the composer row — never over other rows.
    for (rect, hit) in &hits {
        if matches!(hit, Hit::TalkChip) {
            assert_eq!(rect.y as usize, composer_y, "chip on the composer row");
        }
    }
    // And it is EDITABLE: a typed char renders with the cursor.
    model.handle(key(KeyCode::Char('x')));
    let (rows, _, _) = draw(&model, 90, 5);
    assert!(
        rows.iter().any(|row| row.contains("x▮")),
        "typed char renders: {rows:?}"
    );
}

#[test]
fn ninety_by_one_session_has_no_out_of_frame_hits() {
    let mut model = session_model();
    let (rows, hits, _) = draw(&model, 90, 1);
    // The single row is the composer — all chrome shed.
    assert!(
        rows[0].contains('❯') || rows[0].contains('▮'),
        "the one row is the composer: {:?}",
        rows[0]
    );
    assert_hits_in_frame(&hits, 90, 1);
    // Still editable.
    model.handle(key(KeyCode::Char('z')));
    let (rows, hits, _) = draw(&model, 90, 1);
    assert!(rows[0].contains("z▮"), "typed char renders: {:?}", rows[0]);
    assert_hits_in_frame(&hits, 90, 1);
}

#[test]
fn launcher_composer_survives_two_row_frames() {
    let mut model = AppModel::new();
    model.handle(AppEvent::Envelope(Box::new(EventPayload::HarnessStatus(
        haider_protocol::state::HarnessStatus::Ready,
    ))));
    let (rows, hits, _) = draw(&model, 90, 2);
    assert!(
        rows.iter().any(|row| row.contains('❯')),
        "launcher composer visible at 90×2: {rows:?}"
    );
    assert_hits_in_frame(&hits, 90, 2);
    model.handle(key(KeyCode::Char('q')));
    let (rows, _, _) = draw(&model, 90, 2);
    assert!(rows.iter().any(|row| row.contains("q▮")));
}

#[test]
fn ordinary_sizes_keep_their_hits_and_layout() {
    // The seam guard and composer ladder must be invisible at ordinary
    // sizes: the usual hit inventory is intact at 118×34.
    let model = session_model();
    let (rows, hits, _) = draw(&model, 118, 34);
    assert!(rows.iter().any(|row| row.contains("message haider")));
    assert!(rows.iter().any(|row| row.contains("IDLE")), "status intact");
    assert!(hits.iter().any(|(_, h)| matches!(h, Hit::TalkChip)));
    assert!(hits.iter().any(|(_, h)| matches!(h, Hit::BackChip)));
    assert_hits_in_frame(&hits, 118, 34);
}
