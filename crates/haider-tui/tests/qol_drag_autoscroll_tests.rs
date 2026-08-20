//! QoL wave — selection drag autoscroll.
//!
//! While a Left-drag selection is held, a drag event at or past the
//! transcript viewport's top edge scrolls up one line and keeps
//! selecting; at or past the bottom edge it scrolls down. The offset
//! clamps at the content ends (the wheel's reconcile-then-apply), the
//! anchor CELL never moves (screen-space selection law), and the
//! copy-on-release path is untouched.
#![allow(clippy::expect_used)]

use haider_tui::app::{AppEvent, AppModel, AppRequest, Hit};
use haider_tui::mock::demo_script;
use haider_tui::render::render;
use haider_tui::runtime::dispatch_input;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{Event, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

/// A demo session whose transcript overflows a short frame.
fn session_model() -> AppModel {
    let mut model = AppModel::new();
    for payload in demo_script() {
        model.handle(AppEvent::Envelope(Box::new(payload)));
    }
    model
}

fn draw(model: &AppModel, width: u16, height: u16) -> Vec<(Rect, Hit)> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut hits = Vec::new();
    terminal
        .draw(|frame| hits = render(model, frame))
        .expect("draw");
    hits
}

fn mouse(kind: MouseEventKind, column: u16, row: u16) -> Event {
    Event::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    })
}

/// A held selection inside the transcript: Down at (x, y), then one drag
/// a cell away so the selection materializes.
fn start_selection(model: &mut AppModel, hits: &[(Rect, Hit)], x: u16, y: u16) {
    dispatch_input(
        model,
        hits,
        mouse(MouseEventKind::Down(MouseButton::Left), x, y),
    );
    dispatch_input(
        model,
        hits,
        mouse(MouseEventKind::Drag(MouseButton::Left), x + 4, y),
    );
    assert!(
        model.selection.is_some_and(|selection| selection.dragging),
        "the drag armed a live selection"
    );
}

/// MUTATION CHECK: drop the edge test from the Drag arm in
/// `dispatch_input`. Expected runtime failure: a drag event on the
/// viewport's top row leaves `scroll_back` at 0 — the selection stops at
/// the edge instead of scrolling.
#[test]
fn drag_at_the_top_edge_scrolls_up_and_keeps_the_anchor_parked() {
    let model = &mut session_model();
    let hits = draw(model, 90, 12);
    let view = model.transcript_view.get();
    assert!(view.height > 0, "the frame published the viewport");
    assert!(model.scroll_max.get() > 0, "the transcript overflows");
    assert_eq!(model.scroll_back.get(), 0, "tail-follow before the drag");

    let mid = view.y + view.height / 2;
    start_selection(model, &hits, 10, mid);
    let anchor = model.selection.expect("live").anchor;

    // A drag event ON the top edge row scrolls one line and extends.
    dispatch_input(
        model,
        &hits,
        mouse(MouseEventKind::Drag(MouseButton::Left), 8, view.y),
    );
    assert_eq!(model.scroll_back.get(), 1, "one line per drag event");
    let selection = model.selection.expect("still live");
    assert_eq!(selection.anchor, anchor, "the anchor cell never moves");
    assert_eq!(
        selection.head,
        (8, view.y),
        "the lead end follows the pointer"
    );

    // Repeated drag events keep scrolling, clamped at the content start.
    let max = model.scroll_max.get();
    for _ in 0..(max + 5) {
        dispatch_input(
            model,
            &hits,
            mouse(MouseEventKind::Drag(MouseButton::Left), 9, view.y),
        );
    }
    assert_eq!(
        model.scroll_back.get(),
        max,
        "clamped at the content start, no banked debt"
    );
    assert_eq!(model.selection.expect("live").anchor, anchor);
}

/// MUTATION CHECK: drop the `saturating_sub` half of `drag_autoscroll`.
/// Expected runtime failure: a bottom-edge drag leaves the offset parked
/// instead of walking it back toward the tail.
#[test]
fn drag_at_the_bottom_edge_scrolls_down_and_clamps_at_the_tail() {
    let model = &mut session_model();
    let hits = draw(model, 90, 12);
    let view = model.transcript_view.get();
    assert!(model.scroll_max.get() >= 2, "room to scroll");
    // Park the view two lines up, then drag-select toward the bottom.
    model.scroll_back.set(2);
    let mid = view.y + view.height / 2;
    start_selection(model, &hits, 10, mid);

    let bottom = view.y + view.height - 1;
    dispatch_input(
        model,
        &hits,
        mouse(MouseEventKind::Drag(MouseButton::Left), 12, bottom),
    );
    assert_eq!(model.scroll_back.get(), 1, "one line back toward the tail");
    // Past the tail it clamps at 0 — drag events keep arriving, nothing
    // underflows.
    for _ in 0..4 {
        dispatch_input(
            model,
            &hits,
            mouse(MouseEventKind::Drag(MouseButton::Left), 12, bottom + 1),
        );
    }
    assert_eq!(model.scroll_back.get(), 0, "clamped at the tail");
}

#[test]
fn interior_drags_never_scroll_and_release_still_copies() {
    let model = &mut session_model();
    let hits = draw(model, 90, 12);
    let view = model.transcript_view.get();
    let mid = view.y + view.height / 2;
    start_selection(model, &hits, 10, mid);

    // Interior movement extends the selection without touching scroll.
    dispatch_input(
        model,
        &hits,
        mouse(MouseEventKind::Drag(MouseButton::Left), 20, mid + 1),
    );
    assert_eq!(model.scroll_back.get(), 0, "no edge, no scroll");

    // Release resolves exactly as before: auto-copy, highlight kept.
    dispatch_input(
        model,
        &hits,
        mouse(MouseEventKind::Up(MouseButton::Left), 20, mid + 1),
    );
    assert!(
        model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::CopySelection)),
        "copy-on-release is unchanged"
    );
    let selection = model.selection.expect("highlight survives the copy");
    assert!(!selection.dragging, "the drag resolved");
}

#[test]
fn a_drag_with_no_selection_does_not_scroll() {
    let model = &mut session_model();
    let hits = draw(model, 90, 12);
    let view = model.transcript_view.get();
    // Drag events with no armed press (no mouse_down) reach the arm and
    // must do nothing.
    dispatch_input(
        model,
        &hits,
        mouse(MouseEventKind::Drag(MouseButton::Left), 8, view.y),
    );
    assert_eq!(model.scroll_back.get(), 0);
    assert!(model.selection.is_none());
}
