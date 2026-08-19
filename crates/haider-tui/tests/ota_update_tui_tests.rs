//! OTA-on-open TUI contract: discovery enters as data, `/update` emits only
//! shell-owned effects, and a pending release stays quietly visible without
//! interrupting the current surface.
#![allow(clippy::expect_used)]

use haider_tui::app::{AppEvent, AppModel, AppRequest, RuntimeMode, Screen};
use haider_tui::live::LiveDriver;
use haider_tui::render::render;
use haider_tui::runtime::{ShellRequest, live_pass};
use haider_tui::theme::ThemeKey;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Color;

mod common;
use common::{launcher_model, run_slash};

fn live_launcher() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model
}

fn draw(model: &AppModel, width: u16, height: u16) -> Terminal<TestBackend> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            render(model, frame);
        })
        .expect("draw");
    terminal
}

/// MUTATION CHECK: delete the pending-version branch, or accidentally make
/// the request carry daemon/wire behavior. Expected failure: `/update` no
/// longer emits the one shell-owned transaction request.
#[test]
fn pending_release_update_command_requests_the_transaction() {
    let mut model = live_launcher();
    model.update_available = Some("0.0.933".to_owned());

    run_slash(&mut model, "/update");

    assert_eq!(model.requests, vec![AppRequest::RunUpdate]);
    assert_eq!(model.flash.as_deref(), Some("· updating to v0.0.933"));
    assert!(!model.should_quit, "the reducer never exits for an update");
}

/// MUTATION CHECK: treat an unknown startup result as current, or route it
/// straight to the update transaction. Expected failure: the immediate
/// check request is absent or replaced by `RunUpdate`.
#[test]
fn update_command_without_a_pending_release_requests_an_immediate_check() {
    let mut model = live_launcher();

    run_slash(&mut model, "/update");

    assert_eq!(model.requests, vec![AppRequest::CheckForUpdate]);
    assert_eq!(model.flash.as_deref(), Some("· checking for updates…"));
}

#[test]
fn demo_update_command_remains_a_stub_without_shell_effects() {
    let mut model = launcher_model();

    run_slash(&mut model, "/update");

    assert!(model.requests.is_empty());
    assert!(
        model.flash.as_deref().is_some_and(|flash| {
            flash.contains("/update") && flash.contains("live mode installs")
        }),
        "demo honestly names the live-only effect: {:?}",
        model.flash
    );
}

/// MUTATION CHECK: keep the fact only in the one-time flash, hide it on a
/// non-launcher surface, or color it with a raw literal. Expected failure:
/// the persistent text disappears or its ink differs from the active
/// theme's semantic `dim` slot.
#[test]
fn available_event_sets_model_and_persists_a_theme_slot_indicator() {
    for theme_key in ThemeKey::ALL {
        let mut model = live_launcher();
        model.theme = theme_key;
        model.handle(AppEvent::UpdateAvailable {
            version: "v0.0.933".to_owned(),
        });
        assert_eq!(model.update_available.as_deref(), Some("v0.0.933"));
        assert_eq!(
            model.flash.as_deref(),
            Some("· update available — v0.0.933 · /update")
        );

        // The arrival flash is one-time; the underlying fact survives it
        // and follows the user away from the launcher.
        model.flash = None;
        model.screen = Screen::Session;
        let terminal = draw(&model, 110, 30);
        let buffer = terminal.backend().buffer();
        let cell = buffer
            .content()
            .iter()
            .find(|cell| cell.symbol() == "⬆")
            .expect("persistent OTA indicator");
        assert_eq!(
            cell.fg,
            Color::from(theme_key.theme().dim),
            "{theme_key:?} indicator uses the quiet semantic theme slot"
        );
        let text = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("⬆ v0.0.933 — /update"));
    }
}

#[test]
fn current_event_clears_a_stale_release_and_reports_the_running_version() {
    let mut model = live_launcher();
    model.update_available = Some("0.0.933".to_owned());

    model.handle(AppEvent::UpdateCurrent {
        version: "0.0.934".to_owned(),
    });

    assert_eq!(model.update_available, None);
    assert_eq!(model.flash.as_deref(), Some("· up to date — v0.0.934"));
}

/// A failed update is recoverable UI data. It neither clears the pending
/// release (so retry remains possible) nor requests process exit.
#[test]
fn update_failure_flashes_the_typed_error_and_keeps_the_session_alive() {
    let mut model = live_launcher();
    model.update_available = Some("0.0.933".to_owned());

    model.handle(AppEvent::UpdateFailed {
        message: "signature mismatch for haider.tar.gz".to_owned(),
    });

    assert_eq!(
        model.flash.as_deref(),
        Some("· update failed — signature mismatch for haider.tar.gz")
    );
    assert_eq!(model.update_available.as_deref(), Some("0.0.933"));
    assert!(!model.should_quit, "a failed transaction preserves the TUI");
}

#[test]
fn live_pass_maps_both_ota_requests_to_the_closed_shell_vocabulary() {
    let mut model = live_launcher();
    model.requests = vec![AppRequest::CheckForUpdate, AppRequest::RunUpdate];
    let mut driver = LiveDriver::new("ota-test");

    let pass = live_pass(&mut driver, &mut model, None, std::time::Instant::now());

    assert_eq!(
        pass.shell,
        vec![ShellRequest::CheckForUpdate, ShellRequest::RunUpdate]
    );
}
