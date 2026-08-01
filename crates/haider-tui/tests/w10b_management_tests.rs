//! W10b management-polish laws: account/provider removal flows (armed
//! confirm → durable command → typed reply/refusal), the locked edit card,
//! and the HuggingFace preset.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::state::HarnessStatus;
use haider_tui::app::{AccountRow, AppEvent, AppModel, CustomField, RuntimeMode, Screen};
use haider_tui::live::{LiveCommand, LiveDriver, LiveReply};
use haider_tui::runtime::live_pass;
use ratatui::crossterm::event::KeyCode;

mod common;
use common::{key, launcher_model};

fn live_model() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.sessions.clear();
    model
}

fn pass(
    driver: &mut LiveDriver,
    model: &mut AppModel,
    reply: Option<LiveReply>,
) -> Vec<LiveCommand> {
    live_pass(driver, model, reply, std::time::Instant::now()).commands
}

fn account_row(alias: &str, selected: bool) -> AccountRow {
    AccountRow {
        alias: alias.into(),
        provider: "anthropic".into(),
        method: haider_protocol::credential::AuthMethod::ApiKey,
        identity: "id".into(),
        status: haider_protocol::credential::CredentialStatus::Ok,
        selected,
        base_url: None,
    }
}

fn provider_summary(name: &str) -> haider_rpc::ProviderSummaryWire {
    haider_rpc::ProviderSummaryWire {
        provider: name.into(),
        api_family: haider_rpc::ProviderApiFamilyWire::OpenAiChatCompletions,
        endpoint: Some("http://127.0.0.1:9999/v1".into()),
        models: vec!["m-1".into()],
        model_details: Vec::new(),
        auth_methods: Vec::new(),
        availability: haider_rpc::ProviderAvailabilityWire::Available,
        availability_reason: None,
        default_model: Some("m-1".into()),
        enabled: true,
    }
}

/// MUTATION CHECK: drop the `x` arm or the armed-Enter confirm from
/// `handle_accounts_key`. Expected RUNTIME failure: no durable
/// `account.remove` command is issued below.
#[test]
fn account_remove_arms_confirms_and_issues_the_durable_command() {
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    model.accounts.rows = vec![account_row("anthropic", true), account_row("spare", false)];
    model.accounts.revision = Some(7);
    model.accounts.cursor = 1;
    model.screen = Screen::Accounts;

    model.handle(key(KeyCode::Char('x')));
    assert_eq!(model.accounts.pending_remove.as_deref(), Some("spare"));
    // esc disarms without leaving the screen.
    model.handle(key(KeyCode::Esc));
    assert!(model.accounts.pending_remove.is_none());
    assert_eq!(model.screen, Screen::Accounts);

    model.handle(key(KeyCode::Char('x')));
    model.handle(key(KeyCode::Enter));
    let issued = pass(&mut driver, &mut model, None);
    assert!(
        issued.iter().any(|command| matches!(
            command,
            LiveCommand::AccountRemove { alias, expected_revision: Some(7), .. } if alias == "spare"
        )),
        "armed Enter issues the revision-fenced removal: {issued:?}"
    );

    // The committed reply removes the row and surfaces the daemon truth.
    let command_id = issued
        .iter()
        .find_map(|command| match command {
            LiveCommand::AccountRemove { command_id, .. } => Some(command_id.clone()),
            _ => None,
        })
        .expect("command id");
    pass(
        &mut driver,
        &mut model,
        Some(LiveReply::AccountRemoved {
            command_id,
            removed_alias: "spare".into(),
            replacement_active_alias: Some("anthropic".into()),
            revision: 8,
        }),
    );
    assert!(model.accounts.rows.iter().all(|row| row.alias != "spare"));
    assert_eq!(model.accounts.revision, Some(8));
}

/// MUTATION CHECK: drop the provider `x`/Enter arm or the Failed
/// correlation. Expected RUNTIME failure: no `provider.remove` issued, or
/// the daemon's typed refusal never lands on the providers screen.
#[test]
fn provider_remove_issues_and_surfaces_the_typed_refusal() {
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_PROVIDER_REMOVE_V1.to_owned());
    model
        .providers
        .apply_snapshot(vec![provider_summary("probefix")], 3);
    model.screen = Screen::Providers;

    model.handle(key(KeyCode::Char('x')));
    assert_eq!(model.providers.pending_remove.as_deref(), Some("probefix"));
    model.handle(key(KeyCode::Enter));
    let issued = pass(&mut driver, &mut model, None);
    let command_id = issued
        .iter()
        .find_map(|command| match command {
            LiveCommand::ProviderRemove {
                command_id,
                provider,
                expected_revision: 3,
            } if provider == "probefix" => Some(command_id.clone()),
            _ => None,
        })
        .expect("provider.remove issued");

    // The daemon's typed refusal (blocking aliases) surfaces verbatim.
    pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Failed {
            command_id: Some(command_id),
            code: "invalid_argument".into(),
            message: "provider `probefix` is referenced by credential aliases: probefix".into(),
            retryable: false,
        }),
    );
    assert!(
        model.providers.message.as_deref().is_some_and(
            |message| message.contains("not removed") && message.contains("credential aliases")
        ),
        "got {:?}",
        model.providers.message
    );
    assert_eq!(
        model.providers.providers.len(),
        1,
        "a refusal removes nothing"
    );
}

/// MUTATION CHECK: drop the edit lock (focus cycles into identity) or the
/// prefill. Expected RUNTIME failure: the assertions below.
#[test]
fn edit_card_prefills_and_locks_identity() {
    let mut model = live_model();
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_PROVIDER_CONFIGURE_V1.to_owned());
    model
        .providers
        .apply_snapshot(vec![provider_summary("probefix")], 3);
    model.screen = Screen::Providers;

    model.handle(key(KeyCode::Char('e')));
    let card = model.custom_add.as_ref().expect("edit card open");
    assert!(card.edit);
    assert_eq!(card.name, "probefix");
    assert_eq!(card.origin, "http://127.0.0.1:9999/v1");
    assert_eq!(card.model, "m-1");
    assert_eq!(card.focus, CustomField::Model);
    // Tab never leaves the model line in edit mode.
    model.handle(key(KeyCode::Tab));
    assert_eq!(
        model.custom_add.as_ref().expect("card").focus,
        CustomField::Model
    );
}

/// MUTATION CHECK: drop the HuggingFace preset arm. Expected RUNTIME
/// failure: `h` opens nothing (or the origin is not the HF router).
#[test]
fn huggingface_preset_prefills_the_router() {
    let mut model = live_model();
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_PROVIDER_CONFIGURE_V1.to_owned());
    model.providers.apply_snapshot(Vec::new(), 1);
    model.screen = Screen::Providers;
    model.handle(key(KeyCode::Char('h')));
    let card = model.custom_add.as_ref().expect("preset card open");
    assert!(!card.edit);
    assert!(card.name.starts_with("huggingface"));
    assert_eq!(card.origin, "https://router.huggingface.co/v1");
    assert_eq!(card.focus, CustomField::Model);
}

/// MUTATION CHECK: route demo removal to the live request path. Expected
/// RUNTIME failure: the demo removes nothing locally (or promises the
/// daemon a command it cannot serve).
#[test]
fn demo_account_removal_stays_local() {
    let mut model = launcher_model();
    model.handle(AppEvent::Envelope(Box::new(EventPayload::HarnessStatus(
        HarnessStatus::Ready,
    ))));
    model.accounts.rows = vec![account_row("demo-row", true)];
    model.screen = Screen::Accounts;
    model.handle(key(KeyCode::Char('x')));
    model.handle(key(KeyCode::Enter));
    assert!(model.accounts.rows.is_empty(), "demo removal is local");
    assert!(
        model.requests.is_empty(),
        "no live request leaves the demo: {:?}",
        model.requests
    );
}
