#![allow(clippy::expect_used)]
//! v0.0.970 custom providers: credentials precede read-only discovery,
//! selection precedes durable configuration, and discovery can always fall back.

use haider_protocol::credential::AuthMethod;
use haider_tui::app::{
    AccountAddKind, AccountRow, AppEvent, AppModel, AppRequest, CustomField, CustomPhase, Hit,
    RuntimeMode, Screen,
};
use haider_tui::live::{LiveCommand, LiveDriver, LiveReply};
use haider_tui::runtime::live_pass;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

mod tuivirt_common;
use tuivirt_common::{SIZES, check_golden, draw, launcher_model};

fn key(model: &mut AppModel, code: KeyCode) {
    model.handle(AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)));
}

fn live_accounts() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.screen = Screen::Accounts;
    model.sessions.clear();
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_PROVIDER_CONFIGURE_V1.to_owned());
    model.providers.apply_snapshot(Vec::new(), 7);
    model.requests.clear();
    model
}

fn open_custom(model: &mut AppModel) {
    model.handle_hit(Hit::AccountAdd(AccountAddKind::Custom));
}

fn focus(model: &AppModel) -> CustomField {
    model.custom_add.as_ref().expect("open custom card").focus
}

fn probing_model() -> AppModel {
    let mut model = live_accounts();
    open_custom(&mut model);
    key(&mut model, KeyCode::Enter);
    key(&mut model, KeyCode::Enter);
    model.handle(AppEvent::Paste("fixture-api-key".to_owned().into()));
    key(&mut model, KeyCode::Enter);
    assert!(matches!(
        model.custom_add.as_ref().expect("card").phase,
        CustomPhase::Probing
    ));
    model.requests.clear();
    model
}

fn complete_probe(
    model: &mut AppModel,
    models: &[&str],
    default: Option<&str>,
    error: Option<&str>,
) {
    let attempt = model.custom_add.as_ref().expect("card").attempt;
    model.custom_models_probed(
        attempt,
        models.iter().map(|id| (*id).to_owned()).collect(),
        default.map(str::to_owned),
        error.map(str::to_owned),
    );
}

fn saved_account() -> AccountRow {
    AccountRow {
        alias: "local-llama".to_owned(),
        provider: "local-llama".to_owned(),
        method: AuthMethod::ApiKey,
        identity: "saved API key".to_owned(),
        account_identity: None,
        created_at_ms: None,
        status: haider_protocol::credential::CredentialStatus::Ok,
        selected: true,
        base_url: Some("http://127.0.0.1:8000/v1".to_owned()),
    }
}

fn existing_provider() -> haider_rpc::ProviderSummaryWire {
    haider_rpc::ProviderSummaryWire {
        provider: "local-llama".to_owned(),
        api_family: haider_rpc::ProviderApiFamilyWire::OpenAiChatCompletions,
        endpoint: Some("http://127.0.0.1:8000/v1".to_owned()),
        response_open_timeout_ms: None,
        chunk_idle_timeout_ms: None,
        semantic_progress_timeout_ms: None,
        models: vec!["llama3.1:8b".to_owned()],
        model_details: Vec::new(),
        inventory_fetched_at_ms: Some(1_725_000_000_000),
        inventory_authority: haider_rpc::ModelInventoryAuthorityWire::Advisory,
        auth_methods: vec![AuthMethod::ApiKey],
        availability: haider_rpc::ProviderAvailabilityWire::Available,
        availability_reason: None,
        default_model: Some("llama3.1:8b".to_owned()),
        enabled: true,
        trust: haider_rpc::ProviderTrustWire::Full,
    }
}

#[test]
fn generic_enter_order_is_name_origin_key_discovery_model_then_create() {
    let mut model = live_accounts();
    open_custom(&mut model);
    assert_eq!(focus(&model), CustomField::Name);
    key(&mut model, KeyCode::Enter);
    assert_eq!(focus(&model), CustomField::Origin);
    assert!(model.requests.is_empty());
    key(&mut model, KeyCode::Enter);
    assert_eq!(focus(&model), CustomField::Key);
    assert!(
        model.requests.is_empty(),
        "origin cannot configure or probe before key"
    );
    key(&mut model, KeyCode::Enter);
    assert!(
        model.requests.is_empty(),
        "blank new key keeps key entry open"
    );
    model.handle(AppEvent::Paste("fixture-api-key".to_owned().into()));
    key(&mut model, KeyCode::Enter);
    assert!(matches!(
        model.requests.as_slice(),
        [AppRequest::CustomModelsProbe {
            secret: Some(_),
            ..
        }]
    ));
    assert_eq!(model.custom_add.as_ref().expect("card").masked_key_len(), 0);
    model.requests.clear();
    complete_probe(&mut model, &["llama3.1:8b", "qwen2.5:7b"], None, None);
    assert_eq!(focus(&model), CustomField::Model);
    assert_eq!(
        model.custom_add.as_ref().expect("picker").model,
        "llama3.1:8b"
    );
    assert!(
        model.requests.is_empty(),
        "discovery waits for model confirmation"
    );
    key(&mut model, KeyCode::Enter);
    assert!(
        matches!(model.requests.as_slice(), [AppRequest::ProviderConfigure {
        model, models, default_model, expected_revision: 7, secret: None, ..
    }] if model == "llama3.1:8b" && models == &["llama3.1:8b", "qwen2.5:7b"]
        && default_model.as_deref() == Some("llama3.1:8b"))
    );
}

#[test]
fn generic_tab_order_keeps_api_key_before_optional_auth_and_api_family() {
    let mut model = live_accounts();
    open_custom(&mut model);
    for expected in [
        CustomField::Name,
        CustomField::Origin,
        CustomField::Key,
        CustomField::Auth,
        CustomField::ApiFamily,
        CustomField::Name,
    ] {
        assert_eq!(focus(&model), expected);
        key(&mut model, KeyCode::Tab);
    }
    assert!(model.requests.is_empty());
}

#[test]
fn edit_order_locks_name_and_reuses_existing_key_for_discovery() {
    let mut model = live_accounts();
    model.providers.apply_snapshot(vec![existing_provider()], 9);
    model.accounts.rows = vec![saved_account()];
    model.screen = Screen::Providers;
    key(&mut model, KeyCode::Char('e'));
    assert!(
        model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::AccountsRefresh)),
        "edit refreshes credential availability before reusing a saved key"
    );
    model.requests.clear();
    for expected in [
        CustomField::Origin,
        CustomField::Key,
        CustomField::Auth,
        CustomField::Origin,
    ] {
        assert_eq!(focus(&model), expected);
        key(&mut model, KeyCode::Tab);
    }
    // The cycle ends on Key: an empty edit key means reuse the vault entry.
    assert_eq!(focus(&model), CustomField::Key);
    key(&mut model, KeyCode::Enter);
    assert!(
        matches!(model.requests.as_slice(), [AppRequest::CustomModelsProbe {
        name, secret: None, keyless: false, ..
    }] if name == "local-llama")
    );
    let mut driver = LiveDriver::new("customprov-edit");
    let now = std::time::Instant::now();
    let pass = live_pass(&mut driver, &mut model, None, now);
    let attempt = pass
        .commands
        .iter()
        .find_map(|command| match command {
            LiveCommand::ProbeCustomModels {
                attempt,
                probe_vault_reference: None,
                ..
            } => Some(*attempt),
            _ => None,
        })
        .expect("probe uses existing vault key");
    assert!(
        pass.commands
            .iter()
            .all(|command| !matches!(command, LiveCommand::Stage { .. }))
    );
    live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::CustomModelsProbed {
            attempt,
            provider: "local-llama".to_owned(),
            models: vec!["qwen2.5:7b".to_owned(), "llama3.1:8b".to_owned()],
            default_model: Some("qwen2.5:7b".to_owned()),
        }),
        now,
    );
    assert_eq!(
        model.custom_add.as_ref().expect("picker").model,
        "llama3.1:8b",
        "edit keeps a still-served model ahead of the advertised default"
    );
    key(&mut model, KeyCode::Enter);
    let pass = live_pass(&mut driver, &mut model, None, now);
    let command_id = pass
        .commands
        .iter()
        .find_map(|command| match command {
            LiveCommand::ConfigureProvider {
                command_id,
                expected_revision: 9,
                models,
                ..
            } if models.len() == 2 => Some(command_id.clone()),
            _ => None,
        })
        .expect("edit applies full inventory under the observed revision");
    let pass = live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::ProviderConfigured {
            command_id,
            provider: existing_provider(),
            revision: 10,
        }),
        now,
    );
    assert!(pass.commands.iter().all(|command| !matches!(
        command,
        LiveCommand::LoginApi { .. } | LiveCommand::Stage { .. }
    )));
    assert!(model.custom_add.is_none());
    assert!(
        model.login.is_none(),
        "an unchanged edit key never opens a second login"
    );
}

#[test]
fn switching_a_keyless_provider_edit_to_api_key_requires_a_new_key() {
    let mut model = live_accounts();
    let mut provider = existing_provider();
    provider.auth_methods.clear();
    model.providers.apply_snapshot(vec![provider], 9);
    model.screen = Screen::Providers;
    key(&mut model, KeyCode::Char('e'));
    model.requests.clear(); // the opening read is not a model probe or mutation
    key(&mut model, KeyCode::Tab);
    assert_eq!(focus(&model), CustomField::Auth);
    key(&mut model, KeyCode::Char(' '));
    key(&mut model, KeyCode::Enter);
    assert_eq!(focus(&model), CustomField::Key);
    assert!(
        model.requests.is_empty(),
        "changing auth cannot reuse a nonexistent key"
    );
    model.handle(AppEvent::Paste("fixture-api-key".to_owned().into()));
    key(&mut model, KeyCode::Enter);
    assert!(matches!(
        model.requests.as_slice(),
        [AppRequest::CustomModelsProbe {
            secret: Some(_),
            keyless: false,
            ..
        }]
    ));
}

#[test]
fn a_keyed_provider_whose_saved_account_was_removed_requires_a_new_key() {
    let mut model = live_accounts();
    model.providers.apply_snapshot(vec![existing_provider()], 9);
    model.accounts.rows = vec![saved_account()];
    model.screen = Screen::Providers;
    key(&mut model, KeyCode::Char('e'));
    // The account refresh can discover removal while the edit is open.
    model.accounts.apply_snapshot(Vec::new(), Some(10));
    model.requests.clear();
    key(&mut model, KeyCode::Enter); // origin -> key
    key(&mut model, KeyCode::Enter);
    assert_eq!(focus(&model), CustomField::Key);
    assert!(
        model.requests.is_empty(),
        "auth_methods alone is not a saved credential"
    );
    model.handle(AppEvent::Paste("replacement-api-key".to_owned().into()));
    key(&mut model, KeyCode::Enter);
    assert!(matches!(
        model.requests.as_slice(),
        [AppRequest::CustomModelsProbe {
            secret: Some(_),
            keyless: false,
            ..
        }]
    ));
}

#[test]
fn picker_respects_advertised_default_and_arrow_selection_without_editing_ids() {
    let mut model = probing_model();
    complete_probe(
        &mut model,
        &["llama3.1:8b", "qwen2.5:7b"],
        Some("qwen2.5:7b"),
        None,
    );
    assert_eq!(
        model.custom_add.as_ref().expect("picker").model,
        "qwen2.5:7b"
    );
    key(&mut model, KeyCode::Down);
    assert_eq!(
        model.custom_add.as_ref().expect("picker").model,
        "llama3.1:8b"
    );
    key(&mut model, KeyCode::Up);
    key(&mut model, KeyCode::Char('x'));
    assert_eq!(
        model.custom_add.as_ref().expect("picker").model,
        "qwen2.5:7b"
    );
}

#[test]
fn empty_discovery_offers_an_editable_manual_id_with_the_reason() {
    let mut model = probing_model();
    complete_probe(&mut model, &[], None, None);
    assert!(
        matches!(&model.custom_add.as_ref().expect("fallback").phase,
        CustomPhase::Choosing { error: Some(error), models, .. }
        if models.is_empty() && error == "server returned an empty model list — type the model id")
    );
    model.handle(AppEvent::Paste("llama3.1:8b".to_owned().into()));
    key(&mut model, KeyCode::Home);
    key(&mut model, KeyCode::Delete);
    key(&mut model, KeyCode::Char('L'));
    assert_eq!(
        model.custom_add.as_ref().expect("fallback").model,
        "Llama3.1:8b"
    );
    key(&mut model, KeyCode::Enter);
    assert!(
        matches!(model.requests.as_slice(), [AppRequest::ProviderConfigure { model, .. }]
        if model == "Llama3.1:8b")
    );
}

#[test]
fn discovery_success_picker_goldens_at_owner_widths() {
    let mut model = probing_model();
    complete_probe(
        &mut model,
        &["llama3.1:8b", "qwen2.5:7b", "mistral-small"],
        Some("qwen2.5:7b"),
        None,
    );
    for (width, height) in SIZES {
        let frame = draw(&model, width, height);
        assert!(
            frame.contains("llama3.1:8b")
                && frame.contains("qwen2.5:7b")
                && frame.contains("mistral-small")
        );
        assert!(!frame.contains("fixture-api-key"));
        check_golden("customprov_discovery_picker", &frame);
    }
}

#[test]
fn discovery_404_fallback_goldens_keep_the_typed_reason_at_owner_widths() {
    let mut model = probing_model();
    complete_probe(
        &mut model,
        &[],
        None,
        Some("server returned 404 for /models"),
    );
    for (width, height) in SIZES {
        let frame = draw(&model, width, height);
        assert!(
            frame.contains("404")
                && frame.contains("/models")
                && frame.contains("type the model id")
        );
        check_golden("customprov_discovery_404_fallback", &frame);
    }
}
