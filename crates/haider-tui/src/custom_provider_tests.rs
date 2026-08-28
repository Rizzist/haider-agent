#![allow(clippy::expect_used)]

use super::*;
use crate::link::{CommandContext, map_response, request_body};
use crate::live::{LiveCommand, LiveDriver, LiveReply};
use crate::runtime::live_pass;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn live_accounts() -> AppModel {
    let mut model = AppModel::new();
    model.mode = RuntimeMode::Live;
    model.screen = Screen::Accounts;
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_PROVIDER_CONFIGURE_V1.to_owned());
    model.providers.apply_snapshot(Vec::new(), 7);
    model.requests.clear();
    model
}

fn key(model: &mut AppModel, code: KeyCode) {
    model.handle(AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)));
}

fn type_text(model: &mut AppModel, value: &str) {
    for character in value.chars() {
        key(model, KeyCode::Char(character));
    }
}

fn open_custom(model: &mut AppModel) {
    model.handle_hit(Hit::AccountAdd(AccountAddKind::Custom));
}

fn focus_key(model: &mut AppModel) {
    // alias → base URL → auth → API family → masked key
    for _ in 0..4 {
        key(model, KeyCode::Tab);
    }
    assert_eq!(
        model.custom_add.as_ref().expect("custom card").focus,
        CustomField::Key
    );
}

fn discovered_summary(provider: &str) -> haider_rpc::ProviderSummaryWire {
    haider_rpc::ProviderSummaryWire {
        provider: provider.to_owned(),
        api_family: haider_rpc::ProviderApiFamilyWire::OpenAiChatCompletions,
        endpoint: Some("https://router.example.test/v1".to_owned()),
        response_open_timeout_ms: None,
        models: vec!["router-fast".to_owned(), "router-deep".to_owned()],
        model_details: Vec::new(),
        inventory_fetched_at_ms: Some(1_725_000_000_000u64),
        inventory_authority: haider_rpc::ModelInventoryAuthorityWire::Advisory,
        auth_methods: vec![haider_protocol::credential::AuthMethod::ApiKey],
        availability: haider_rpc::ProviderAvailabilityWire::Available,
        availability_reason: None,
        default_model: Some("router-fast".to_owned()),
        enabled: true,
        trust: haider_rpc::ProviderTrustWire::Full,
    }
}

#[test]
fn custom_server_key_is_masked_and_debug_redacted() {
    const SENTINEL: &str = "CUSTOM_TUI_SECRET_SENTINEL_4f21";
    let mut model = live_accounts();
    open_custom(&mut model);
    focus_key(&mut model);
    type_text(&mut model, SENTINEL);

    let card = model.custom_add.as_ref().expect("card stays open");
    assert_eq!(card.masked_key_len(), SENTINEL.chars().count());
    assert!(!format!("{card:?}").contains(SENTINEL));

    let backend = TestBackend::new(110, 32);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| drop(crate::render::render(&model, frame)))
        .expect("draw");
    let buffer = terminal.backend().buffer();
    let mut rendered = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            rendered.push_str(buffer[(x, y)].symbol());
        }
    }
    assert!(
        !rendered.contains(SENTINEL),
        "a frame must never contain the key"
    );
    assert!(rendered.contains("••••"), "the entry is visibly masked");

    key(&mut model, KeyCode::BackTab); // API family
    key(&mut model, KeyCode::BackTab); // auth
    key(&mut model, KeyCode::Char(' ')); // no auth
    assert_eq!(
        model
            .custom_add
            .as_ref()
            .expect("card stays open")
            .masked_key_len(),
        0,
        "switching to no auth wipes the abandoned key"
    );
}

#[test]
fn no_auth_and_anthropic_choices_configure_without_a_secret_or_placeholder_model() {
    let mut model = live_accounts();
    let mut driver = LiveDriver::new("custom-no-auth-test");
    open_custom(&mut model);
    key(&mut model, KeyCode::Tab); // base URL
    key(&mut model, KeyCode::Tab); // auth
    key(&mut model, KeyCode::Char(' ')); // no auth
    key(&mut model, KeyCode::Tab); // API family
    key(&mut model, KeyCode::Right); // anthropic
    key(&mut model, KeyCode::Enter);

    let pass = live_pass(&mut driver, &mut model, None, std::time::Instant::now());
    let command = pass
        .commands
        .into_iter()
        .find(|command| matches!(command, LiveCommand::ConfigureProvider { .. }))
        .expect("no-auth configure is direct");
    match &command {
        LiveCommand::ConfigureProvider {
            keyless,
            family,
            model,
            models,
            probe_vault_reference,
            ..
        } => {
            assert!(*keyless);
            assert_eq!(
                *family,
                haider_rpc::ProviderApiFamilyWire::AnthropicMessages
            );
            assert!(model.is_empty());
            assert!(models.is_empty());
            assert!(probe_vault_reference.is_none());
        }
        _ => unreachable!(),
    }
    match request_body(command) {
        haider_rpc::RequestBody::ProviderConfigure {
            auth_requirement,
            models,
            default_model,
            probe_vault_reference,
            ..
        } => {
            assert_eq!(
                auth_requirement,
                Some(haider_rpc::ProviderAuthRequirementWire::None)
            );
            assert!(models.is_empty(), "live discovery receives no fake model");
            assert!(default_model.is_none());
            assert!(probe_vault_reference.is_none());
        }
        other => panic!("unexpected body: {other:?}"),
    }
}

#[test]
fn keyed_custom_server_stages_configures_discovers_and_consumes_one_reference() {
    const SENTINEL: &str = "CUSTOM_TUI_STAGE_SENTINEL_b718";
    let mut model = live_accounts();
    let mut driver = LiveDriver::new("custom-key-test");
    let now = std::time::Instant::now();
    open_custom(&mut model);
    focus_key(&mut model);
    type_text(&mut model, SENTINEL);
    key(&mut model, KeyCode::Enter);

    let staged = live_pass(&mut driver, &mut model, None, now);
    let stage = staged
        .commands
        .iter()
        .find(|command| matches!(command, LiveCommand::Stage { .. }))
        .expect("key is staged before configure");
    assert!(!format!("{stage:?}").contains(SENTINEL));
    let (provider, alias, attempt) = match stage {
        LiveCommand::Stage {
            provider,
            alias,
            attempt,
            ..
        } => (provider.clone(), alias.clone(), *attempt),
        _ => unreachable!(),
    };
    assert_eq!(provider, "custom");
    assert_eq!(alias.as_deref(), Some("custom"));

    let configured = live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Staged {
            vault_reference: "vault-ref-custom-1".to_owned(),
            provider,
            alias,
            attempt,
        }),
        now,
    );
    let configure = configured
        .commands
        .into_iter()
        .find(|command| matches!(command, LiveCommand::ConfigureProvider { .. }))
        .expect("stage reply mints configure");
    let configure_id = configure.command_id().expect("durable configure").clone();
    match request_body(configure) {
        haider_rpc::RequestBody::ProviderConfigure {
            api_family,
            auth_requirement,
            models,
            default_model,
            probe_vault_reference,
            ..
        } => {
            assert_eq!(
                api_family,
                Some(haider_rpc::ProviderApiFamilyWire::OpenAiChatCompletions)
            );
            assert_eq!(
                auth_requirement,
                Some(haider_rpc::ProviderAuthRequirementWire::ApiKey)
            );
            assert!(models.is_empty());
            assert!(default_model.is_none());
            assert_eq!(probe_vault_reference.as_deref(), Some("vault-ref-custom-1"));
        }
        other => panic!("unexpected body: {other:?}"),
    }

    let login = live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::ProviderConfigured {
            command_id: configure_id,
            provider: discovered_summary("custom"),
            revision: 8,
        }),
        now,
    );
    let login = login
        .commands
        .iter()
        .find(|command| matches!(command, LiveCommand::LoginApi { .. }))
        .expect("configure chains account login");
    match login {
        LiveCommand::LoginApi {
            provider,
            alias,
            vault_reference,
            ..
        } => {
            assert_eq!(provider, "custom");
            assert_eq!(alias.as_deref(), Some("custom"));
            assert_eq!(vault_reference, "vault-ref-custom-1");
        }
        _ => unreachable!(),
    }
    assert!(matches!(
        model.login.as_ref().map(|card| &card.stage),
        Some(LoginStage::Submitting)
    ));
    assert!(
        model
            .model_picker_rows()
            .iter()
            .any(|row| row.provider == "custom" && row.model == "router-deep")
    );
}

#[test]
fn typed_stage_failure_reopens_the_exact_card_and_requires_a_key_retype() {
    let mut model = live_accounts();
    let mut driver = LiveDriver::new("custom-error-test");
    let now = std::time::Instant::now();
    open_custom(&mut model);
    focus_key(&mut model);
    type_text(&mut model, "throw-away-key");
    key(&mut model, KeyCode::Enter);
    let staged = live_pass(&mut driver, &mut model, None, now);
    let attempt = staged
        .commands
        .iter()
        .find_map(|command| match command {
            LiveCommand::Stage { attempt, .. } => Some(*attempt),
            _ => None,
        })
        .expect("stage issued");

    live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::StageFailed {
            attempt,
            code: "unauthorized".to_owned(),
            message: "server rejected the API key".to_owned(),
        }),
        now,
    );
    let card = model.custom_add.as_ref().expect("same card reopens");
    assert_eq!(card.focus, CustomField::Key);
    assert_eq!(card.masked_key_len(), 0, "submitted key was wiped");
    assert!(matches!(
        &card.phase,
        CustomPhase::Editing { error: Some(error) }
            if error.contains("unauthorized") && error.contains("rejected")
    ));
}

#[test]
fn typed_probe_failure_class_survives_link_mapping_and_surfaces_on_the_card() {
    let mut model = live_accounts();
    let mut driver = LiveDriver::new("custom-probe-error-test");
    let now = std::time::Instant::now();
    open_custom(&mut model);
    focus_key(&mut model);
    type_text(&mut model, "throw-away-key");
    key(&mut model, KeyCode::Enter);
    let staged = live_pass(&mut driver, &mut model, None, now);
    let (provider, alias, attempt) = staged
        .commands
        .iter()
        .find_map(|command| match command {
            LiveCommand::Stage {
                provider,
                alias,
                attempt,
                ..
            } => Some((provider.clone(), alias.clone(), *attempt)),
            _ => None,
        })
        .expect("stage issued");
    let configured = live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Staged {
            vault_reference: "vault-ref-probe-error".to_owned(),
            provider,
            alias,
            attempt,
        }),
        now,
    );
    let configure = configured
        .commands
        .into_iter()
        .find(|command| matches!(command, LiveCommand::ConfigureProvider { .. }))
        .expect("configure issued");
    let context = CommandContext::of(&configure);
    let mapped = map_response(
        &context,
        haider_rpc::ResponseBody::Error {
            code: "provider_error".to_owned(),
            message: "GET /v1/models returned 401".to_owned(),
            retryable: false,
            data: Some(haider_rpc::ErrorData::ProviderProbeFailed {
                provider: "custom".to_owned(),
                failure: haider_rpc::ProviderProbeFailureWire::Unauthorized,
            }),
        },
    );
    assert!(matches!(
        mapped.as_slice(),
        [LiveReply::ProviderProbeFailed {
            provider,
            failure: haider_rpc::ProviderProbeFailureWire::Unauthorized,
            ..
        }] if provider == "custom"
    ));

    let reply = mapped.into_iter().next().expect("one typed reply");
    live_pass(&mut driver, &mut model, Some(reply), now);
    let card = model.custom_add.as_ref().expect("card reopens");
    assert_eq!(card.focus, CustomField::Key);
    assert!(matches!(
        &card.phase,
        CustomPhase::Editing { error: Some(error) }
            if error.contains("API key unauthorized") && error.contains("401")
    ));
}

fn rendered_text(model: &AppModel, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| drop(crate::render::render(model, frame)))
        .expect("draw");
    let buffer = terminal.backend().buffer();
    let mut rendered = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            rendered.push_str(buffer[(x, y)].symbol());
        }
        rendered.push('\n');
    }
    rendered
}

/// MUTATION CHECK: drop any of the three lock markers or create a second
/// status bar. Expected failure: registry, picker, or the existing status
/// strip stops exposing the same provider-scoped trust fact.
#[test]
fn lockdown_provider_marks_registry_picker_and_existing_status_strip() {
    let mut model = AppModel::new();
    model.mode = RuntimeMode::Live;
    model.screen = Screen::Providers;
    model.identity.provider = "research".into();
    model.identity.model_short = "router-fast".into();
    let mut summary = discovered_summary("research");
    summary.trust = haider_rpc::ProviderTrustWire::Lockdown;
    model.providers.apply_snapshot(vec![summary], 8);

    assert!(model.model_picker_rows().iter().all(|row| row.lockdown));
    assert!(crate::render::status_left_string(&model, 180).contains("🔒 lockdown · research"));
    let rendered = rendered_text(&model, 120, 34);
    assert!(rendered.contains("🔒 lockdown"));
}

/// MUTATION CHECK: derive the status chip directly from the mutable provider
/// roster. Expected failure: toggling Full removes the lock before the next
/// accepted turn boundary.
#[test]
fn lockdown_status_chip_changes_only_at_a_turn_boundary() {
    let mut model = AppModel::new();
    model.identity.provider = "research".into();
    let mut lockdown = discovered_summary("research");
    lockdown.trust = haider_rpc::ProviderTrustWire::Lockdown;
    model.providers.apply_snapshot(vec![lockdown], 8);
    model.note_lockdown_turn_boundary();
    assert_eq!(model.active_lockdown_provider(), Some("research"));

    let full = discovered_summary("research");
    model.providers.apply_snapshot(vec![full], 9);
    assert_eq!(
        model.active_lockdown_provider(),
        Some("research"),
        "a roster toggle applies to the following turn"
    );

    model.note_lockdown_turn_boundary();
    assert_eq!(model.active_lockdown_provider(), None);
}

#[test]
fn provider_list_and_model_picker_both_offer_the_trust_toggle() {
    let mut model = AppModel::new();
    model.mode = RuntimeMode::Live;
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_PROVIDER_LOCKDOWN_V1.to_owned());
    model
        .providers
        .apply_snapshot(vec![discovered_summary("research")], 8);
    model.screen = Screen::Providers;
    model.requests.clear();
    key(&mut model, KeyCode::Char('t'));
    assert!(matches!(
        model.requests.pop(),
        Some(AppRequest::ProviderSetTrust {
            ref provider,
            trust: haider_rpc::ProviderTrustWire::Lockdown,
            expected_revision: 8,
        }) if provider == "research"
    ));

    model.open_model_picker(String::new());
    model.requests.clear();
    key(&mut model, KeyCode::Tab);
    assert!(matches!(
        model.requests.pop(),
        Some(AppRequest::ProviderSetTrust {
            ref provider,
            trust: haider_rpc::ProviderTrustWire::Lockdown,
            expected_revision: 8,
        }) if provider == "research"
    ));
}

#[test]
fn lockdown_overlay_and_refusal_keep_their_own_visual_taxonomy() {
    let mut model = AppModel::new();
    model.screen = Screen::Session;
    model.identity.provider = "research".into();
    model.lockdown_overlay = true;
    model.lockdown_status = Some(haider_rpc::LockdownStatusWire {
        provider: Some("research".into()),
        tools_allowed: vec!["fs_read".into(), "web_search".into()],
        quota_used: 4_096,
        quota_limit: 1_073_741_824,
    });
    model
        .projection
        .apply(&haider_protocol::EventPayload::LockdownRefused(
            haider_protocol::lockdown::LockdownRefused {
                provider: "research".into(),
                tool: "process_exec".into(),
                reason: "outside the fixed envelope".into(),
                tools_allowed: vec!["fs_read".into(), "web_search".into()],
            },
        ));

    let plain = crate::plain::render_plain(&model.projection, 0, None);
    assert!(plain.contains("🔒 REFUSED · research · process_exec"));
    let rendered = rendered_text(&model, 120, 34);
    assert!(rendered.contains("provider lockdown"));
    assert!(rendered.contains("global quota"));
    assert!(rendered.contains("4.0 KiB / 1.00 GiB"));
    assert!(rendered.contains("refused"));
    assert!(!rendered.contains("✗ process_exec"));
}
