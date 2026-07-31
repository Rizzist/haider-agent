//! W5g-4 — the `+ Custom (OpenAI-compatible)` card: sim-verbatim demo
//! fabrication, and the live `provider.configure` front door with
//! editable name/origin fields that chains into the masked key card.
#![allow(clippy::expect_used)]

use haider_protocol::credential::AuthMethod;
use haider_tui::app::{AppEvent, AppModel, CustomField, CustomPhase, Hit, LoginFocus, RuntimeMode};
use haider_tui::live::{LiveCommand, LiveDriver, LiveReply};
use haider_tui::runtime::live_pass;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

mod common;
use common::{launcher_model, run_slash};

fn key(model: &mut AppModel, code: KeyCode) {
    model.handle(AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)));
}

fn type_text(model: &mut AppModel, text: &str) {
    for c in text.chars() {
        key(model, KeyCode::Char(c));
    }
}

fn open_card(model: &mut AppModel) {
    model.handle_hit(Hit::AccountAdd(haider_tui::app::AccountAddKind::Custom));
}

fn accounts_model() -> AppModel {
    let mut model = launcher_model();
    run_slash(&mut model, "/accounts");
    model.requests.clear();
    model
}

fn live_summary(provider: &str) -> haider_rpc::ProviderSummaryWire {
    haider_rpc::ProviderSummaryWire {
        provider: provider.to_owned(),
        api_family: haider_rpc::ProviderApiFamilyWire::OpenAiChatCompletions,
        endpoint: Some("http://127.0.0.1:9999/v1".to_owned()),
        models: Vec::new(),
        model_details: Vec::new(),
        auth_methods: vec![AuthMethod::ApiKey],
        availability: haider_rpc::ProviderAvailabilityWire::Unavailable,
        availability_reason: Some("provider model inventory is unavailable".to_owned()),
        default_model: None,
        enabled: true,
    }
}

/// MUTATION CHECK (W5g-4): make the demo `[1]` land a row under the BARE
/// `custom` provider instead of the sim's `custom-N`/`local-N` recipe.
/// Expected runtime failure: every assertion on the fabricated row below.
#[test]
fn the_demo_card_is_the_sims_fabrication() {
    let mut model = accounts_model();
    open_card(&mut model);
    assert!(model.custom_add.is_some(), "demo card opens");

    key(&mut model, KeyCode::Char('1'));
    assert!(model.custom_add.is_none(), "confirm closes the card");
    let row = model
        .accounts
        .rows
        .iter()
        .find(|row| row.provider == "custom-1")
        .expect("sim recipe row lands");
    assert_eq!(row.alias, "local-1");
    assert_eq!(row.base_url.as_deref(), Some("http://127.0.0.1:8000/v1"));
    assert!(row.selected, "the new account is active (sim confirmAuth)");
    assert!(
        model
            .accounts
            .message
            .as_deref()
            .is_some_and(|message| message.contains("✓ added custom-1 · local-1")),
        "the sim's ✓ message"
    );
    // `[2]` cancels without fabricating.
    open_card(&mut model);
    key(&mut model, KeyCode::Char('2'));
    assert!(model.custom_add.is_none());
    assert_eq!(
        model
            .accounts
            .rows
            .iter()
            .filter(|r| r.provider.starts_with("custom-"))
            .count(),
        1
    );
}

/// MUTATION CHECK (W5g-4): drop `expected_revision` from the submit (send
/// 0). Expected runtime failure: the captured `ConfigureProvider` below
/// carries the stale-proof revision no longer.
#[test]
fn the_live_card_edits_and_submits_under_the_snapshot_revision() {
    let mut model = accounts_model();
    model.mode = RuntimeMode::Live;
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_PROVIDER_CONFIGURE_V1.to_owned());
    model.providers.apply_snapshot(Vec::new(), 7);
    let mut driver = LiveDriver::new("test");

    open_card(&mut model);
    {
        let card = model.custom_add.as_ref().expect("live card opens");
        assert_eq!(card.name, "custom", "prefilled provider id");
        assert_eq!(card.origin, "http://127.0.0.1:8000/v1");
        assert_eq!(card.focus, CustomField::Name);
    }
    type_text(&mut model, "-llama");
    key(&mut model, KeyCode::Tab);
    for _ in 0.."8000/v1".len() {
        key(&mut model, KeyCode::Backspace);
    }
    type_text(&mut model, "11434/v1");
    // An empty model REFUSES the submit — an enabled create without an
    // inventory and default would bounce at the daemon (W5g-5).
    key(&mut model, KeyCode::Enter);
    {
        let card = model.custom_add.as_ref().expect("card");
        assert!(matches!(card.phase, CustomPhase::Editing { .. }));
        assert_eq!(card.focus, CustomField::Model, "the missing field focuses");
    }
    type_text(&mut model, "llama3.1:8b");
    key(&mut model, KeyCode::Enter);
    assert!(matches!(
        model.custom_add.as_ref().expect("card").phase,
        CustomPhase::Submitting
    ));

    let pass = live_pass(&mut driver, &mut model, None, std::time::Instant::now());
    assert!(
        pass.commands.iter().any(|command| matches!(
            command,
            LiveCommand::ConfigureProvider { provider, origin, model, expected_revision, .. }
                if provider == "custom-llama"
                    && origin == "http://127.0.0.1:11434/v1"
                    && model == "llama3.1:8b"
                    && *expected_revision == 7
        )),
        "the configure rides the edited fields under the snapshot revision"
    );
}

/// MUTATION CHECK (W5g-4): make `custom_add_committed` close the card
/// WITHOUT opening the key card. Expected runtime failure: the login-card
/// assertion below — a provider without a credential is a dead end.
#[test]
fn a_committed_configure_chains_into_the_key_card() {
    let mut model = accounts_model();
    model.mode = RuntimeMode::Live;
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_PROVIDER_CONFIGURE_V1.to_owned());
    model.providers.apply_snapshot(Vec::new(), 7);
    let mut driver = LiveDriver::new("test");

    open_card(&mut model);
    key(&mut model, KeyCode::Tab);
    key(&mut model, KeyCode::Tab);
    type_text(&mut model, "llama3.1:8b");
    key(&mut model, KeyCode::Enter);
    let pass = live_pass(&mut driver, &mut model, None, std::time::Instant::now());
    let command_id = pass
        .commands
        .iter()
        .find_map(|command| match command {
            LiveCommand::ConfigureProvider { command_id, .. } => Some(command_id.clone()),
            _ => None,
        })
        .expect("configure issued");

    live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::ProviderConfigured {
            command_id,
            provider: live_summary("custom"),
            revision: 8,
        }),
        std::time::Instant::now(),
    );
    assert!(model.custom_add.is_none(), "the card closed on commit");
    assert!(
        model
            .providers
            .providers
            .iter()
            .any(|summary| summary.provider == "custom"),
        "the created profile joined the registry snapshot"
    );
    let login = model.login.as_ref().expect("the key card opened — chained");
    assert_eq!(login.provider, "custom");
    assert_eq!(login.focus, LoginFocus::Key);
}

/// MUTATION CHECK (W5g-4): drop the error correlation (never call
/// `custom_add_failed`). Expected runtime failure: the card below stays
/// stuck in `Submitting` instead of reopening its fields with the reason.
#[test]
fn a_failed_configure_reopens_the_fields_with_the_reason() {
    let mut model = accounts_model();
    model.mode = RuntimeMode::Live;
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_PROVIDER_CONFIGURE_V1.to_owned());
    model.providers.apply_snapshot(Vec::new(), 7);
    let mut driver = LiveDriver::new("test");

    open_card(&mut model);
    key(&mut model, KeyCode::Tab);
    key(&mut model, KeyCode::Tab);
    type_text(&mut model, "llama3.1:8b");
    key(&mut model, KeyCode::Enter);
    let pass = live_pass(&mut driver, &mut model, None, std::time::Instant::now());
    let command_id = pass
        .commands
        .iter()
        .find_map(|command| match command {
            LiveCommand::ConfigureProvider { command_id, .. } => Some(command_id.clone()),
            _ => None,
        })
        .expect("configure issued");

    live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Failed {
            command_id: Some(command_id),
            code: "invalid_argument".to_owned(),
            message: "origin must be loopback".to_owned(),
            retryable: false,
        }),
        std::time::Instant::now(),
    );
    let card = model
        .custom_add
        .as_ref()
        .expect("card survives the failure");
    assert!(matches!(
        &card.phase,
        CustomPhase::Editing { error: Some(error) } if error == "origin must be loopback"
    ));
    assert_eq!(card.origin, "http://127.0.0.1:8000/v1", "fields kept");
}

/// The two accounts-screen cards are mutually exclusive — neither can
/// open over the other (each would fight for the same key routing).
#[test]
fn the_cards_are_mutually_exclusive() {
    let mut model = accounts_model();
    open_card(&mut model);
    assert!(model.custom_add.is_some());
    model.handle_hit(Hit::AccountAdd(
        haider_tui::app::AccountAddKind::OpenAiOAuth,
    ));
    assert!(model.oauth_add.is_none(), "oauth card refused over custom");

    let mut model = accounts_model();
    model.handle_hit(Hit::AccountAdd(
        haider_tui::app::AccountAddKind::OpenAiOAuth,
    ));
    assert!(model.oauth_add.is_some());
    open_card(&mut model);
    assert!(model.custom_add.is_none(), "custom card refused over oauth");
}

/// MUTATION CHECK (W5g-4): offer the card without the daemon feature gate
/// (report §4.1: never offer a method the daemon cannot serve). Expected
/// runtime failure: the stale-daemon assertion below.
#[test]
fn a_stale_daemon_gets_the_note_not_the_card() {
    let mut model = accounts_model();
    model.mode = RuntimeMode::Live;
    open_card(&mut model);
    assert!(model.custom_add.is_none(), "no card without the feature");
    assert!(
        model
            .accounts
            .message
            .as_deref()
            .is_some_and(|message| { message.contains("daemon") || message.contains("upgrade") }),
        "the stale-daemon note explains why"
    );
}

/// MUTATION CHECK (W5g-5 live fix): drop the login-card block from
/// `render_accounts`. Expected runtime failure: the chained key card is
/// an INVISIBLE total-modal trap — open, eating every keystroke, drawn by
/// no frame (the live probe's exact failure; the `+ … (API)` buttons had
/// the same latent bug since W5d).
#[test]
fn the_chained_key_card_is_visible_on_the_accounts_screen() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let mut model = accounts_model();
    model.mode = RuntimeMode::Live;
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_PROVIDER_CONFIGURE_V1.to_owned());
    model.providers.apply_snapshot(Vec::new(), 7);
    let mut driver = LiveDriver::new("test");
    open_card(&mut model);
    key(&mut model, KeyCode::Tab);
    key(&mut model, KeyCode::Tab);
    type_text(&mut model, "probe-model");
    key(&mut model, KeyCode::Enter);
    let pass = live_pass(&mut driver, &mut model, None, std::time::Instant::now());
    let command_id = pass
        .commands
        .iter()
        .find_map(|command| match command {
            LiveCommand::ConfigureProvider { command_id, .. } => Some(command_id.clone()),
            _ => None,
        })
        .expect("configure issued");
    live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::ProviderConfigured {
            command_id,
            provider: live_summary("custom"),
            revision: 8,
        }),
        std::time::Instant::now(),
    );
    assert!(model.login.is_some(), "the key card chained");

    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            haider_tui::render::render(&model, frame);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let mut text = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            text.push_str(buffer[(x, y)].symbol());
        }
        text.push('\n');
    }
    assert!(
        text.contains("API key") && text.contains("alias ❯"),
        "the key card renders on the accounts screen: {}",
        text.lines().rev().take(10).collect::<Vec<_>>().join("\n")
    );
}

/// MUTATION CHECK (W5g-5 live fix): restore the OAuth-only gate in the
/// driver's `provider_model_refreshes`. Expected runtime failure: the
/// selected API-KEY account below never asks for its catalog — the live
/// probe's exact starvation (a custom provider's models can only come
/// from its origin, and nothing else triggers that fetch).
#[test]
fn a_selected_api_key_account_triggers_model_discovery() {
    let mut model = accounts_model();
    model.mode = RuntimeMode::Live;
    let mut driver = LiveDriver::new("test");
    // The trigger runs on the ACCOUNTS reply path — feed the api-key
    // account through it (a pre-seeded row would be replaced).
    let descriptor = haider_protocol::credential::CredentialDescriptor {
        alias: haider_protocol::ids::CredentialAlias::new("probe-api"),
        provider: "custom-llama".to_owned(),
        base_url: Some("http://127.0.0.1:18123/v1".to_owned()),
        auth_method: AuthMethod::ApiKey,
        identity: "sk-… probe".to_owned(),
        status: haider_protocol::credential::CredentialStatus::Ok,
        active: true,
    };
    let pass = live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Accounts {
            descriptors: vec![descriptor],
            revision: Some(2),
        }),
        std::time::Instant::now(),
    );
    let accounts_pass = pass;
    let providers_pass = live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Providers {
            providers: vec![live_summary("custom-llama")],
            revision: 3,
        }),
        std::time::Instant::now(),
    );
    // The trigger fires on whichever snapshot lands the starving account
    // first (dedup keeps it to ONE request per connection).
    let asked = accounts_pass
        .commands
        .iter()
        .chain(providers_pass.commands.iter())
        .filter(|command| {
            matches!(
                command,
                LiveCommand::RefreshProviderModels { provider } if provider == "custom-llama"
            )
        })
        .count();
    assert_eq!(asked, 1, "an api-key account's empty catalog asks ONCE");
}
