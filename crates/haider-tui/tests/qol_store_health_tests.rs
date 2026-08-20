//! QoL wave — store health surfaced honestly (`store_health_v1`).
//!
//! The daemon latches durable-store write failures (journal appends
//! included) and pushes the transition to every connection as an
//! out-of-band `ProtocolError` — degraded (`store_full` /
//! `store_read_only` / `store_unavailable`) and recovered
//! (`store_healthy`) alike — and replays the latched state to a client
//! connecting while degraded. These pins own the CLIENT half of that
//! contract end to end: the wire frame maps to `ProfileDiagnostic`, the
//! persistent banner paints on every screen and survives redraws and
//! screen switches, and only the healthy edge clears it. The transition
//! latch itself is pinned beside the store
//! (`haider-core/src/sqlite_store.rs`); the Welcome advertisement rides
//! the daemon's exact-set pin (`connection_tests.rs`).
#![allow(clippy::expect_used)]

use haider_rpc::{ProtocolError, WireFrame};
use haider_tui::app::{AppEvent, AppModel, Hit, RuntimeMode, Screen};
use haider_tui::link::map_frame;
use haider_tui::live::{LiveDriver, LiveReply};
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::Rect;

mod common;
use common::{key, launcher_model};

fn sid() -> haider_protocol::ids::SessionId {
    haider_protocol::ids::SessionId::new("s-health")
}

fn live_session() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.sessions.clear();
    model.upsert_live_session(&sid());
    model.open_session(&sid());
    model
}

fn draw_text(model: &AppModel, width: u16, height: u16) -> String {
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
        .collect::<Vec<_>>()
        .join("\n")
}

/// The daemon watcher's degraded frame, byte-shaped like
/// `profile_store_fault_frame` (subcode dashes become the code's
/// underscores; the presentation rides whole).
fn degraded_frame(code: &str, subcode: &str) -> WireFrame {
    WireFrame::ProtocolError(ProtocolError {
        code: code.to_owned(),
        message: "Store unwritable — profile disk is full.".to_owned(),
        fatal: false,
        presentation: Some(haider_protocol::error::ErrorPresentation::new(
            subcode,
            "Store unwritable",
            "Store unwritable — profile disk is full. Not committed: event-9. \
             Free space or restore write access, then retry.",
            haider_protocol::error::ErrorScope::Profile,
            [haider_protocol::error::ErrorAction::Retry],
        )),
        failed_write_ids: vec!["event-9".to_owned()],
    })
}

/// The daemon watcher's recovery frame — code only, no presentation.
fn healthy_frame() -> WireFrame {
    WireFrame::ProtocolError(ProtocolError {
        code: "store_healthy".to_owned(),
        message: "profile store is writable again".to_owned(),
        fatal: false,
        presentation: None,
        failed_write_ids: Vec::new(),
    })
}

/// MUTATION CHECK: drop the store codes from `map_frame`'s ProtocolError
/// arm. Expected runtime failure: the degraded frame maps to a transient
/// `Failed` flash instead of the persistent `ProfileDiagnostic`, and no
/// banner survives the next keypress.
#[test]
fn every_degraded_code_maps_to_the_persistent_diagnostic() {
    for (code, subcode) in [
        ("store_full", "store-full"),
        ("store_read_only", "store-read-only"),
        ("store_unavailable", "store-unavailable"),
    ] {
        let replies = map_frame(degraded_frame(code, subcode));
        let [
            LiveReply::ProfileDiagnostic {
                card: Some(haider_protocol::menu::ErrorRecoveryCardKind::StoreUnwritable),
                presentation: Some(presentation),
                failed_write_ids,
            },
        ] = replies.as_slice()
        else {
            panic!("{code} must map to ProfileDiagnostic, got {replies:?}");
        };
        assert_eq!(presentation.subcode.as_str(), subcode);
        assert_eq!(failed_write_ids, &["event-9".to_owned()]);
    }
}

/// MUTATION CHECK: drop the clear-on-`store_healthy` half (map the
/// healthy frame to nothing). Expected runtime failure: the banner never
/// clears — the last assertion still sees "Store unwritable" after
/// recovery.
#[test]
fn banner_paints_while_degraded_survives_redraws_and_clears_on_healthy() {
    let mut model = live_session();
    let mut driver = LiveDriver::new("health");
    for reply in map_frame(degraded_frame("store_full", "store-full")) {
        driver.apply(&mut model, reply);
    }
    // Painted on the session…
    let text = draw_text(&model, 120, 20);
    assert!(text.contains("✗ Store unwritable"), "{text}");
    assert!(text.contains("store-full"), "the subcode rides the facts");
    // …survives any number of redraws…
    for _ in 0..3 {
        let text = draw_text(&model, 120, 20);
        assert!(text.contains("✗ Store unwritable"));
    }
    // …and screen switches (back to the launcher), while the session
    // keeps running — a warning line, never a modal or an input block.
    model.handle(AppEvent::Key(ratatui::crossterm::event::KeyEvent::new(
        KeyCode::Char('c'),
        ratatui::crossterm::event::KeyModifiers::CONTROL,
    )));
    assert_eq!(model.screen, Screen::Launcher);
    let text = draw_text(&model, 120, 20);
    assert!(text.contains("✗ Store unwritable"), "{text}");
    model.handle(key(KeyCode::Char('x')));
    assert_eq!(model.composer, "x", "input still flows beneath the banner");

    // The healthy edge clears the banner everywhere.
    for reply in map_frame(healthy_frame()) {
        driver.apply(&mut model, reply);
    }
    assert!(model.profile_diagnostic.is_none());
    let text = draw_text(&model, 120, 20);
    assert!(!text.contains("Store unwritable"), "{text}");
}

/// The client half of attach-while-degraded: the daemon replays the
/// latched fault to a connection made while degraded, so the frame can
/// arrive BEFORE any session is attached — parked on the launcher — and
/// must still raise the banner.
#[test]
fn a_degraded_snapshot_arriving_pre_attach_still_raises_the_banner() {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    assert_eq!(model.screen, Screen::Launcher);
    let mut driver = LiveDriver::new("health-attach");
    for reply in map_frame(degraded_frame("store_read_only", "store-read-only")) {
        driver.apply(&mut model, reply);
    }
    let text = draw_text(&model, 120, 20);
    assert!(text.contains("✗ Store unwritable"), "{text}");
    assert!(text.contains("store-read-only"), "{text}");
}
