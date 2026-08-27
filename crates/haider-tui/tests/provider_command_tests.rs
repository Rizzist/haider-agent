#![allow(clippy::expect_used)]

//! `/provider <name>` ownership: client-owned at the launcher, DAEMON TRUTH
//! in an attached session. The in-session arm used to assign
//! `identity.provider` locally and flash success, so the TUI announced a
//! switch the daemon never heard and the next turn still ran on the old
//! provider.

use haider_protocol::ids::SessionId;
use haider_tui::app::{AppModel, AppRequest, RuntimeMode, Screen};

fn provider_summary(
    provider: &str,
    models: Vec<String>,
    default_model: Option<String>,
) -> haider_rpc::ProviderSummaryWire {
    haider_rpc::ProviderSummaryWire {
        provider: provider.to_owned(),
        api_family: haider_rpc::ProviderApiFamilyWire::OpenAiChatCompletions,
        endpoint: None,
        response_open_timeout_ms: None,
        model_details: Vec::new(),
        models,
        inventory_fetched_at_ms: None,
        inventory_authority: haider_rpc::ModelInventoryAuthorityWire::Unknown,
        auth_methods: Vec::new(),
        availability: haider_rpc::ProviderAvailabilityWire::Available,
        availability_reason: None,
        default_model,
        enabled: true,
    }
}

fn run_line(model: &mut AppModel, line: &str) {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    for c in line.chars() {
        model.handle(haider_tui::app::AppEvent::Key(KeyEvent::new(
            KeyCode::Char(c),
            KeyModifiers::NONE,
        )));
    }
    // Esc dismisses the palette so ⏎ runs the typed line, not a palette row.
    model.handle(haider_tui::app::AppEvent::Key(KeyEvent::new(
        KeyCode::Esc,
        KeyModifiers::NONE,
    )));
    model.handle(haider_tui::app::AppEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
}

/// Two providers, the second carrying whatever default the caller wants.
fn model_with_providers(default_model: Option<String>) -> AppModel {
    let mut model = AppModel::new();
    model.mode = RuntimeMode::Live;
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_SESSION_MODEL_SELECT_V1.to_owned());
    model.providers.apply_snapshot(
        vec![
            provider_summary(
                "openai",
                vec!["gpt-5.6".to_owned()],
                Some("gpt-5.6".to_owned()),
            ),
            provider_summary("anthropic", vec!["claude-x".to_owned()], default_model),
        ],
        1,
    );
    model.identity.provider = "openai".to_owned();
    model
}

fn selected_model_for<'a>(model: &'a AppModel, provider: &str) -> Option<&'a str> {
    model.requests.iter().find_map(|request| match request {
        AppRequest::SelectModel {
            provider: p, model, ..
        } if p == provider => Some(model.as_str()),
        _ => None,
    })
}

#[test]
fn provider_switch_in_a_live_session_is_receipted_not_local() {
    // The whole point: an attached session's provider is daemon truth, so
    // `/provider` must COMMIT it rather than repaint the status bar.
    let mut model = model_with_providers(Some("claude-x".to_owned()));
    model.screen = Screen::Session;
    model.active_session = Some(SessionId::new("s-1"));

    run_line(&mut model, "/provider anthropic");

    assert_eq!(
        selected_model_for(&model, "anthropic"),
        Some("claude-x"),
        "the switch must reach the daemon as a receipted select: {:?}",
        model.requests
    );
    // Daemon truth is rendered by `apply_model_selected` when the RESOLVED
    // pair comes back. Echoing the request locally is what made the old
    // behaviour a lie rather than merely incomplete.
    assert_eq!(
        model.identity.provider, "openai",
        "the local identity must not pre-empt the daemon's resolved pair"
    );
}

#[test]
fn provider_without_a_default_model_refuses_rather_than_guessing() {
    // A provider is committed by selecting a MODEL on it. When the daemon
    // declares no default, reaching for the first catalog entry would commit
    // a choice the daemon never sanctioned — the same lie one line down.
    let mut model = model_with_providers(None);
    model.screen = Screen::Session;
    model.active_session = Some(SessionId::new("s-1"));

    run_line(&mut model, "/provider anthropic");

    assert_eq!(
        selected_model_for(&model, "anthropic"),
        None,
        "no default model must mean no invented commit: {:?}",
        model.requests
    );
    let flash = model.flash.clone().unwrap_or_default();
    assert!(
        flash.contains("no default model"),
        "the refusal must say why, not fail silently: {flash:?}"
    );
    assert_eq!(
        model.identity.provider, "openai",
        "a refused switch must not move the local identity either"
    );
}

#[test]
fn provider_at_the_launcher_stays_client_owned() {
    // At the launcher there is no session to mutate — `/provider` pins the
    // default pair the next CreateSession mints. This arm is client-owned and
    // must NOT start issuing receipted selects.
    let mut model = model_with_providers(Some("claude-x".to_owned()));
    model.screen = Screen::Launcher;
    model.active_session = None;

    run_line(&mut model, "/provider anthropic");

    assert_eq!(
        model.identity.provider, "anthropic",
        "the launcher pins the next session's provider locally"
    );
    assert_eq!(
        selected_model_for(&model, "anthropic"),
        None,
        "there is no session to select on: {:?}",
        model.requests
    );
}
