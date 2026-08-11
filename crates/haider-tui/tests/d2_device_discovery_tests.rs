//! Device discovery is daemon policy now. The TUI only triggers the
//! discovery/auto-adopt cadence and then refreshes roster truth; it never
//! renders candidates or offers manual import controls.
#![allow(clippy::expect_used)]

use haider_tui::app::{AppModel, AppRequest, RuntimeMode, Screen};
use haider_tui::live::{LiveCommand, LiveDriver, LiveReply};
use haider_tui::mock::{seed_account_rows, seed_provider_summaries};
use haider_tui::render::render;
use haider_tui::runtime::live_pass;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

mod common;
use common::{launcher_model, run_slash};

fn live_model(features: &[&str]) -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.daemon_features = features.iter().map(|name| (*name).to_owned()).collect();
    model.daemon_version = Some("0.0.902".to_owned());
    model.accounts.apply_snapshot(seed_account_rows(), Some(1));
    model.providers.apply_snapshot(seed_provider_summaries(), 1);
    model
}

fn draw(model: &AppModel) -> String {
    let backend = TestBackend::new(118, 40);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            let _ = render(model, frame);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer();
    let mut text = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            text.push_str(buffer[(x, y)].symbol());
        }
        text.push('\n');
    }
    text
}

#[test]
fn accounts_refresh_triggers_daemon_auto_adopt_but_has_no_device_section() {
    let mut model = live_model(&[haider_rpc::FEATURE_ACCOUNT_DEVICE_DISCOVERY_V1]);
    let mut driver = LiveDriver::new("test");
    run_slash(&mut model, "/accounts");
    assert_eq!(model.screen, Screen::Accounts);
    let issued = live_pass(&mut driver, &mut model, None, std::time::Instant::now()).commands;
    assert!(issued.contains(&LiveCommand::DeviceCandidates));
    assert!(!draw(&model).contains("found on this device"));
}

#[test]
fn discovery_completion_refreshes_roster_truth_not_candidate_chrome() {
    let mut model = live_model(&[haider_rpc::FEATURE_ACCOUNT_DEVICE_DISCOVERY_V1]);
    let mut driver = LiveDriver::new("test");
    let followups = driver.apply(
        &mut model,
        LiveReply::DeviceCandidates {
            discovery_disabled: false,
            candidates: Vec::new(),
        },
    );
    assert_eq!(
        followups,
        vec![LiveCommand::AccountList, LiveCommand::ProviderList],
        "the daemon may have auto-adopted rows; materialize both rosters"
    );
    assert!(!draw(&model).contains("found on this device"));
}

#[test]
fn ungated_and_demo_modes_never_request_device_discovery() {
    for mut model in [live_model(&[]), launcher_model()] {
        if model.mode == RuntimeMode::Demo {
            assert_eq!(model.mode, RuntimeMode::Demo);
        }
        run_slash(&mut model, "/accounts");
        assert!(
            !model
                .requests
                .iter()
                .any(|request| matches!(request, AppRequest::DeviceCandidatesRefresh))
        );
        assert!(!draw(&model).contains("found on this device"));
    }
}
