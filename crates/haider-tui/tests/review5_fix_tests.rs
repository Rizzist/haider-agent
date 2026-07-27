//! Review-round-5 fix guards: chrome yields progressively to a blocking
//! menu (status row → session line → header rule → input rule → product
//! line), the option viewport at 1-2-row floors (⋮ markers, ↑↓ window,
//! digit answers), and wheel reconcile-then-apply burst behavior through
//! production dispatch.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_tui::app::{AppEvent, AppModel, Hit};
use haider_tui::mock::demo_script;
use haider_tui::render::render;
use haider_tui::runtime::dispatch_input;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{Event, KeyCode, KeyModifiers, MouseEvent, MouseEventKind};
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

fn wheel(up: bool) -> Event {
    Event::Mouse(MouseEvent {
        kind: if up {
            MouseEventKind::ScrollUp
        } else {
            MouseEventKind::ScrollDown
        },
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    })
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

fn option_hit(hits: &[(Rect, Hit)], index: usize) -> Option<Hit> {
    hits.iter().find_map(|(_, h)| match h {
        Hit::MenuOption { index: i, .. } if *i == index => Some(h.clone()),
        _ => None,
    })
}

// ---- P2-1: chrome yields to a blocking menu ----

#[test]
fn menu_options_survive_ninety_by_six_with_status_shed() {
    let mut model = menu_model();
    let (rows, hits, _) = draw(&model, 90, 6);
    // Both options render; the STATUS ROW yielded first.
    let allow_y = rows
        .iter()
        .position(|row| row.contains("1. Allow once"))
        .expect("allow rendered");
    assert!(rows.iter().any(|row| row.contains("2. Deny")));
    assert!(
        !rows.iter().any(|row| row.contains("PERMISSION_REQUIRED")),
        "status row shed before any option"
    );
    // The header product line survives at this height.
    assert!(rows.iter().any(|row| row.contains("haider")));
    let _ = allow_y;
    for index in 0..2 {
        assert!(
            option_hit(&hits, index).is_some(),
            "option {index} clickable"
        );
    }
    model.handle_hit(option_hit(&hits, 1).expect("deny"));
    assert_eq!(
        model.outbox.pop().expect("answer").option_key.as_deref(),
        Some("deny")
    );
}

#[test]
fn menu_options_survive_ninety_by_five_with_session_line_shed() {
    let mut model = menu_model();
    let (rows, hits, _) = draw(&model, 90, 5);
    assert!(rows.iter().any(|row| row.contains("1. Allow once")));
    assert!(rows.iter().any(|row| row.contains("2. Deny")));
    // Status row and the header's SESSION line both shed.
    assert!(!rows.iter().any(|row| row.contains("PERMISSION_REQUIRED")));
    assert!(
        !rows.iter().any(|row| row.contains("branch main")),
        "header line 2 shed before any option"
    );
    for index in 0..2 {
        assert!(
            option_hit(&hits, index).is_some(),
            "option {index} clickable"
        );
    }
    model.handle_hit(option_hit(&hits, 0).expect("allow"));
    assert_eq!(
        model.outbox.pop().expect("answer").option_key.as_deref(),
        Some("allow")
    );
}

#[test]
fn menu_options_survive_ninety_by_two_with_all_chrome_shed() {
    let model = menu_model();
    let (rows, hits, _) = draw(&model, 90, 2);
    assert!(rows.iter().any(|row| row.contains("1. Allow once")));
    assert!(rows.iter().any(|row| row.contains("2. Deny")));
    // Every piece of chrome yielded: no header, no rules, no status.
    assert!(!rows.iter().any(|row| row.contains("haider")));
    assert!(!rows.iter().any(|row| row.contains('─')));
    assert!(!rows.iter().any(|row| row.contains("PERMISSION_REQUIRED")));
    for index in 0..2 {
        assert!(
            option_hit(&hits, index).is_some(),
            "option {index} clickable"
        );
    }
}

#[test]
fn one_row_floor_windows_options_with_markers() {
    let mut model = menu_model();
    // 90×1: fewer rows than options — the viewport shows the selection
    // with a ⋮ marker; every option stays reachable and answerable.
    let (rows, hits, _) = draw(&model, 90, 1);
    assert!(
        rows[0].contains("1. Allow once"),
        "selected option rendered: {:?}",
        rows[0]
    );
    assert!(rows[0].contains('⋮'), "hidden neighbor marked");
    assert!(option_hit(&hits, 0).is_some(), "rendered option clickable");
    assert!(
        option_hit(&hits, 1).is_none(),
        "hidden option has no phantom hit"
    );
    // ↑↓ moves the window: Down reveals option 2 (still marked — option 1
    // is now hidden above).
    model.handle(key(KeyCode::Down));
    let (rows, hits, _) = draw(&model, 90, 1);
    assert!(
        rows[0].contains("2. Deny"),
        "window followed: {:?}",
        rows[0]
    );
    assert!(rows[0].contains('⋮'));
    assert!(option_hit(&hits, 1).is_some());
    // Digits answer ANY option, rendered or not.
    model.handle(key(KeyCode::Up));
    let (_, _, _) = draw(&model, 90, 1);
    model.handle(key(KeyCode::Char('2')));
    assert_eq!(
        model
            .outbox
            .pop()
            .expect("digit answer")
            .option_key
            .as_deref(),
        Some("deny"),
        "digit reaches the hidden option"
    );
}

// ---- P2-2: wheel reconcile-then-apply, burst through production dispatch ----

#[test]
fn wheel_burst_reversal_moves_the_view_through_production_dispatch() {
    let mut model = session_model();
    let (_, _, _) = draw(&model, 90, 14);
    let max = model.scroll_max.get();
    assert!(max > 0);
    // The reviewer's repro: 100 queued wheel-ups then ONE wheel-down, no
    // frame in between — all through the production input path.
    for _ in 0..100 {
        dispatch_input(&mut model, &[], wheel(true));
    }
    assert_eq!(model.scroll_back.get(), max, "burst banks no debt");
    dispatch_input(&mut model, &[], wheel(false));
    assert_eq!(
        model.scroll_back.get(),
        max.saturating_sub(3),
        "the down-notch moves the view"
    );
    // The frame confirms exactly that — nothing hidden to repay.
    let (_, _, _) = draw(&model, 90, 14);
    assert_eq!(model.scroll_back.get(), max.saturating_sub(3));
}
