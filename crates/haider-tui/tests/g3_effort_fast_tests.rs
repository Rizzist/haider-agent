//! G3 — `/effort` + `/fast` per provider-model pair.
//!
//! The laws:
//! * LE7 (IdentityLine writer): after an effort-select REPLY the composer
//!   rule shows `· <effort>`; after `/fast` on a supported pair it shows
//!   `· fast` — including fast ALONE with no explicit effort.
//! * bare `/effort` opens the ladder picker fed by DAEMON truth
//!   (ModelDetailWire — the TUI holds no tables); an empty-ladder pair
//!   refuses honestly.
//! * `/fast` refuses CLIENT-SIDE on a pair whose detail declares no `fast`
//!   speed (decision 6: client refusal AND daemon refusal), and pushes the
//!   receipted toggle on a supported pair.
//! * live selections render daemon truth from the correlated reply — never
//!   an echo of the request, never optimism.
#![allow(clippy::expect_used)]

use haider_protocol::ids::SessionId;
use haider_rpc::ModelDetailWire;
use haider_tui::app::{AppModel, AppRequest, RuntimeMode, Screen};
use haider_tui::live::{LiveCommand, LiveDriver, LiveReply};
use haider_tui::mock::{seed_account_rows, seed_provider_summaries};
use haider_tui::runtime::live_pass;
use ratatui::crossterm::event::KeyCode;

mod common;
use common::{key, launcher_model, run_slash};

fn sid() -> SessionId {
    SessionId::new("g3-effort-session")
}

/// Seed summaries with the anthropic pair's DAEMON-projected tuning detail:
/// claude-opus-5 gets the full ladder + the fast speed; claude-sonnet-5
/// gets the ladder but NO fast speed.
fn tuned_summaries() -> Vec<haider_rpc::ProviderSummaryWire> {
    let mut summaries = seed_provider_summaries();
    let anthropic = summaries
        .iter_mut()
        .find(|summary| summary.provider == "anthropic")
        .expect("anthropic summary");
    anthropic.model_details = vec![
        ModelDetailWire {
            name: "claude-opus-5".into(),
            display_name: None,
            context_window: Some(1_000_000),
            supported_efforts: ["low", "medium", "high", "xhigh", "max"]
                .map(str::to_owned)
                .to_vec(),
            default_effort: Some("high".into()),
            supported_speeds: vec!["fast".into()],
            supports_thinking_type: None,
            supports_vision: None,
        },
        ModelDetailWire {
            name: "claude-sonnet-5".into(),
            display_name: None,
            context_window: Some(1_000_000),
            supported_efforts: ["low", "medium", "high", "xhigh", "max"]
                .map(str::to_owned)
                .to_vec(),
            default_effort: Some("high".into()),
            supported_speeds: Vec::new(),
            supports_thinking_type: None,
            supports_vision: None,
        },
    ];
    summaries
}

fn seeded_session() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.daemon_features = [
        "provider_models_v1",
        "session_model_select_v1",
        "session_effort_select_v1",
        "session_fast_select_v1",
    ]
    .iter()
    .map(|name| (*name).to_owned())
    .collect();
    model.daemon_version = Some("0.0.72".to_owned());
    model.providers.apply_snapshot(tuned_summaries(), 1);
    model.accounts.apply_snapshot(seed_account_rows(), Some(1));
    model.identity.provider = "anthropic".to_owned();
    model.identity.model_short = "claude-opus-5".to_owned();
    model.identity_pinned = true;
    model.sessions.clear();
    model.upsert_live_session(&sid());
    model.open_session(&sid());
    assert_eq!(model.screen, Screen::Session);
    model.requests.clear();
    model
}

fn pass(
    driver: &mut LiveDriver,
    model: &mut AppModel,
    reply: Option<LiveReply>,
) -> Vec<LiveCommand> {
    live_pass(driver, model, reply, std::time::Instant::now()).commands
}

/// LAW (LE7): the identity's tuning segment is written from correlated
/// daemon replies — `· xhigh` after the effort reply, `· xhigh · fast`
/// after the fast reply, and `· fast` ALONE after an effort revert — with
/// NO optimistic write at request time.
///
/// MUTATION CHECK (executed — see the G3 mutation notes): write
/// `identity.reasoning` at request time in `request_effort`. Expected
/// runtime failure: the no-optimism assertion below.
#[test]
fn effort_and_fast_replies_write_the_identity_line() {
    let mut model = seeded_session();
    let mut driver = LiveDriver::new("test");

    run_slash(&mut model, "/effort xhigh");
    assert!(
        model
            .composer_identity(120)
            .is_none_or(|line| !line.contains("xhigh")),
        "no optimism: the identity holds until the reply"
    );
    let commands = pass(&mut driver, &mut model, None);
    let (command_id, session) = commands
        .iter()
        .find_map(|command| match command {
            LiveCommand::SelectEffort {
                command_id,
                session,
                effort,
                ..
            } => {
                assert_eq!(effort.as_deref(), Some("xhigh"));
                Some((command_id.clone(), session.clone()))
            }
            _ => None,
        })
        .expect("session.select_effort issued");
    assert_eq!(session, sid());

    pass(
        &mut driver,
        &mut model,
        Some(LiveReply::EffortSelected {
            command_id,
            session: sid(),
            effort: Some("xhigh".into()),
            worker_generation: 1,
        }),
    );
    let line = model.composer_identity(120).expect("identity line");
    assert!(
        line.contains("· xhigh"),
        "LE7: the composer rule shows the selected effort: {line}"
    );
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|flash| flash.contains("cache re-warm")),
        "anthropic pairs note the prompt-cache re-warm: {:?}",
        model.flash
    );

    run_slash(&mut model, "/fast");
    let commands = pass(&mut driver, &mut model, None);
    let command_id = commands
        .iter()
        .find_map(|command| match command {
            LiveCommand::SelectFast {
                command_id,
                enabled,
                ..
            } => {
                assert!(*enabled, "the toggle requests ON from off");
                Some(command_id.clone())
            }
            _ => None,
        })
        .expect("session.select_fast issued");
    pass(
        &mut driver,
        &mut model,
        Some(LiveReply::FastSelected {
            command_id,
            session: sid(),
            enabled: true,
            worker_generation: 1,
        }),
    );
    let line = model.composer_identity(120).expect("identity line");
    assert!(
        line.contains("· xhigh · fast"),
        "LE7: fast rides the effort segment: {line}"
    );

    // Reverting the effort keeps the fast marker ALONE — the segment
    // exists when EITHER knob is set.
    model.apply_effort_selected(None);
    let line = model.composer_identity(120).expect("identity line");
    assert!(
        line.contains("· fast") && !line.contains("xhigh"),
        "LE7: fast renders alone after an effort revert: {line}"
    );
}

/// Bare `/effort` opens the picker with `default` leading the DAEMON
/// ladder; the provider default and current selection are marked; an
/// argument outside the ladder refuses with the ladder named.
#[test]
fn bare_effort_opens_the_ladder_picker_from_daemon_truth() {
    let mut model = seeded_session();
    run_slash(&mut model, "/effort");
    assert!(model.effort_picker.is_some(), "the picker opens");
    let rows = model.effort_picker_rows();
    assert_eq!(rows.len(), 6, "default + the five declared levels");
    assert_eq!(rows[0].effort, None);
    assert!(
        rows[0].is_current,
        "nothing pinned → the default row is current"
    );
    assert_eq!(rows[3].effort.as_deref(), Some("high"));
    assert!(
        rows[3].is_provider_default,
        "the daemon-declared default is marked"
    );

    model.effort_picker = None;
    run_slash(&mut model, "/effort ultra");
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|flash| flash.contains("not in this pair's ladder")),
        "an out-of-ladder argument refuses naming the ladder: {:?}",
        model.flash
    );
    assert!(
        model.requests.is_empty(),
        "a client-refused level never reaches the wire"
    );
}

/// An empty-ladder pair refuses `/effort` honestly — the TUI holds no
/// tables and invents nothing.
#[test]
fn empty_ladder_pair_refuses_effort_honestly() {
    let mut model = seeded_session();
    model.identity.provider = "openai".to_owned();
    model.identity.model_short = "gpt-5.6".to_owned();
    run_slash(&mut model, "/effort");
    assert!(
        model.effort_picker.is_none(),
        "no picker over an empty ladder"
    );
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|flash| flash.contains("declares no effort ladder")),
        "the refusal names the pair: {:?}",
        model.flash
    );
}

/// LAW (LE4, client half): `/fast` on a pair whose daemon detail declares
/// no `fast` speed refuses CLIENT-SIDE (no request); the supported pair
/// pushes the receipted toggle; and turning OFF always goes through.
#[test]
fn fast_gate_refuses_client_side_on_unsupported_pairs() {
    let mut model = seeded_session();
    model.identity.model_short = "claude-sonnet-5".to_owned();
    run_slash(&mut model, "/fast");
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|flash| flash.contains("does not support fast mode")),
        "the client refusal names the pair: {:?}",
        model.flash
    );
    assert!(
        model.requests.is_empty(),
        "an unsupported enable never reaches the wire"
    );

    model.identity.model_short = "claude-opus-5".to_owned();
    run_slash(&mut model, "/fast");
    assert!(
        matches!(
            model.requests.last(),
            Some(AppRequest::SelectFast { enabled: true, .. })
        ),
        "the supported pair pushes the receipted toggle: {:?}",
        model.requests
    );

    // Turning OFF is always allowed — even after a switch off the gate.
    model.requests.clear();
    model.identity.fast = true;
    model.identity.model_short = "claude-sonnet-5".to_owned();
    run_slash(&mut model, "/fast");
    assert!(
        matches!(
            model.requests.last(),
            Some(AppRequest::SelectFast { enabled: false, .. })
        ),
        "disable always goes through: {:?}",
        model.requests
    );
}

/// The picker commits through the keyboard: ⏎ on a highlighted level
/// issues the request and marks it pending; esc closes without selecting.
#[test]
fn picker_enter_commits_and_esc_closes() {
    let mut model = seeded_session();
    run_slash(&mut model, "/effort");
    assert!(model.effort_picker.is_some());
    model.handle(key(KeyCode::Esc));
    assert!(
        model.effort_picker.is_none(),
        "esc closes without selecting"
    );
    assert!(model.requests.is_empty());

    run_slash(&mut model, "/effort");
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Enter));
    assert!(
        matches!(
            model.requests.last(),
            Some(AppRequest::SelectEffort { effort: Some(level), .. }) if level == "low"
        ),
        "⏎ commits the highlighted level: {:?}",
        model.requests
    );
    let picker = model
        .effort_picker
        .as_ref()
        .expect("picker holds while pending");
    assert_eq!(picker.pending, Some(Some("low".to_owned())));
}
