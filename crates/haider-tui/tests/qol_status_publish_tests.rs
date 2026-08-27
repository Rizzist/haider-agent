//! QoL wave — the W-INP STATUS mirror (`status_segment_v1`).
//!
//! The status bar's bottom-left strip is ONE pure composition
//! (`render::status_left_segments`): the frame styles it, the live
//! tail's `status_publish_pass` publishes its joined string — so mirror
//! and screen can never diverge — under the composer publisher's exact
//! dedup discipline (epoch-keyed cache, monotonic revisions, publish
//! only on change).
#![allow(clippy::expect_used)]

use haider_tui::app::{AppEvent, AppModel, Hit, RuntimeMode, Screen};
use haider_tui::live::LiveCommand;
use haider_tui::render::{render, status_left_string};
use haider_tui::runtime::status_publish_pass;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;

mod common;
use common::launcher_model;

fn sid() -> haider_protocol::ids::SessionId {
    haider_protocol::ids::SessionId::new("s-status")
}

fn live_session() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_STATUS_SEGMENT_V1.to_owned());
    model.sessions.clear();
    model.upsert_live_session(&sid());
    model.open_session(&sid());
    assert_eq!(model.screen, Screen::Session);
    model
}

#[test]
fn provider_open_wait_surfaces_elapsed_and_remaining_budget() {
    let mut model = live_session();
    model
        .projection
        .apply(&haider_protocol::EventPayload::RunState(
            haider_protocol::state::RunState::Thinking,
        ));
    model.provider_wait_started_at_ms = Some(1_000);
    model.clock_ms = 13_000;

    let strip = status_left_string(&model, 180);
    assert!(
        strip.contains("waiting for provider · 12s elapsed · 48s left"),
        "the existing status strip makes the default 60s provider wait visible: {strip}"
    );
}

fn draw_rows(model: &AppModel, width: u16, height: u16) -> Vec<String> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut hits: Vec<(Rect, Hit)> = Vec::new();
    terminal
        .draw(|frame| hits = render(model, frame))
        .expect("draw");
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect()
}

fn published(command: &LiveCommand) -> (&str, u64) {
    let LiveCommand::SurfacePublish {
        input: None,
        status: Some((line, _state, _detail, revision)),
        ..
    } = command
    else {
        panic!("a status-only SurfacePublish, got {command:?}");
    };
    (line.as_str(), *revision)
}

/// MUTATION CHECK: make `render_status_bar` compose its own spans instead
/// of consuming `status_left_segments`. Expected runtime failure: the
/// painted status row and the published string drift apart and this
/// equality breaks.
#[test]
fn the_published_string_equals_the_rendered_strip() {
    let model = live_session();
    let rows = draw_rows(&model, 100, 14);
    let width = model.status_width.get();
    assert_eq!(width, 100, "the frame published its status width");
    let strip = status_left_string(&model, width);
    assert!(!strip.trim().is_empty(), "the strip carries the state word");
    // The session status row has no right-side hint, so the row is the
    // strip plus pad spaces — trailing pad is the only difference.
    let status_row = rows
        .iter()
        .find(|row| row.starts_with(strip.trim_end()))
        .unwrap_or_else(|| panic!("no rendered row carries the strip {strip:?}"));
    assert_eq!(status_row.trim_end(), strip.trim_end());

    // The publisher publishes exactly that string.
    let mut cache = None;
    let mut revision = 0;
    let command =
        status_publish_pass(&model, 1, &mut cache, &mut revision).expect("first pass publishes");
    let (line, rev) = published(&command);
    assert_eq!(line, strip);
    assert_eq!(rev, 1);
}

/// MUTATION CHECK: drop the `published == current` compare from
/// `status_publish_pass`. Expected runtime failure: the second pass
/// republishes an unchanged strip and the revision churns to 2.
#[test]
fn publishes_only_on_change_with_monotonic_revisions() {
    let mut model = live_session();
    let _ = draw_rows(&model, 100, 14);
    let mut cache = None;
    let mut revision = 0;

    assert!(
        status_publish_pass(&model, 1, &mut cache, &mut revision).is_some(),
        "first value publishes"
    );
    assert!(
        status_publish_pass(&model, 1, &mut cache, &mut revision).is_none(),
        "an unchanged strip never republishes"
    );

    // A real strip change (queue mode joins the branch segment) publishes
    // once, with the next revision.
    model.queue_mode = true;
    let command = status_publish_pass(&model, 1, &mut cache, &mut revision)
        .expect("the changed strip publishes");
    let (line, rev) = published(&command);
    assert!(
        line.contains("q:turn"),
        "the new strip rode the wire: {line}"
    );
    assert_eq!(rev, 2);
    assert!(
        status_publish_pass(&model, 1, &mut cache, &mut revision).is_none(),
        "and dedups again"
    );
}

/// MUTATION CHECK: key the cache on (session, text) without the epoch.
/// Expected runtime failure: after a redial the daemon's cleared surface
/// never hears the unchanged strip again.
#[test]
fn a_new_connection_epoch_republishes_the_same_strip() {
    let model = live_session();
    let _ = draw_rows(&model, 100, 14);
    let mut cache = None;
    let mut revision = 0;
    let first =
        status_publish_pass(&model, 1, &mut cache, &mut revision).expect("first epoch publishes");
    let command =
        status_publish_pass(&model, 2, &mut cache, &mut revision).expect("the redial republishes");
    assert_eq!(published(&first).0, published(&command).0, "same strip");
    assert_eq!(published(&command).1, 2, "revision stays monotonic");
}

#[test]
fn ungated_daemon_wrong_screen_and_unrendered_bar_publish_nothing() {
    // Feature-ungated: the daemon never advertised status_segment_v1.
    let mut ungated = live_session();
    let _ = draw_rows(&ungated, 100, 14);
    ungated.daemon_features.clear();
    let mut cache = None;
    let mut revision = 0;
    assert!(status_publish_pass(&ungated, 1, &mut cache, &mut revision).is_none());

    // The launcher is not a bound session surface.
    let mut parked = live_session();
    let _ = draw_rows(&parked, 100, 14);
    parked.handle(AppEvent::Key(ratatui::crossterm::event::KeyEvent::new(
        ratatui::crossterm::event::KeyCode::Char('c'),
        ratatui::crossterm::event::KeyModifiers::CONTROL,
    )));
    assert_eq!(parked.screen, Screen::Launcher);
    assert!(status_publish_pass(&parked, 1, &mut cache, &mut revision).is_none());

    // A bar that has never rendered (width 0) has no on-screen strip to
    // mirror yet.
    let fresh = live_session();
    assert_eq!(fresh.status_width.get(), 0);
    assert!(status_publish_pass(&fresh, 1, &mut cache, &mut revision).is_none());
}
