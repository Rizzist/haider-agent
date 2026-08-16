//! W10b management-polish laws: account/provider removal flows (armed
//! confirm → durable command → typed reply/refusal), the locked edit card,
//! and the HuggingFace preset.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::state::HarnessStatus;
use haider_tui::app::{
    AccountRow, AppEvent, AppModel, AppRequest, CustomField, RuntimeMode, Screen,
};
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
            presentation: None,
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

/// MUTATION CHECK: relock the origin in edit mode (Tab pins back to Model),
/// let Tab/BackTab land on the identity line, or drop the prefill. Expected
/// RUNTIME failure: the assertions below — the endpoint becomes unreachable
/// for repointing, or the fixed name takes focus.
#[test]
fn edit_card_prefills_and_repoints_endpoint_with_locked_name() {
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
    // The endpoint is repointable in place: Tab reaches the origin line…
    model.handle(key(KeyCode::Tab));
    assert_eq!(
        model.custom_add.as_ref().expect("card").focus,
        CustomField::Origin
    );
    // …and cycles back to the model, never onto the locked identity line.
    model.handle(key(KeyCode::Tab));
    assert_eq!(
        model.custom_add.as_ref().expect("card").focus,
        CustomField::Model
    );
    // BackTab mirrors the two-field cycle and also stays off the name line.
    model.handle(key(KeyCode::BackTab));
    assert_eq!(
        model.custom_add.as_ref().expect("card").focus,
        CustomField::Origin
    );
}

/// MUTATION CHECK: send the ORIGINAL (unedited) origin on an edit submit,
/// or block char input on the origin line in edit mode. Expected RUNTIME
/// failure: the emitted `provider.configure` carries the stale origin (or
/// the origin never changes), so the repoint never reaches the daemon.
#[test]
fn edit_card_submits_repointed_origin_under_the_same_name() {
    let mut model = live_model();
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_PROVIDER_CONFIGURE_V1.to_owned());
    model
        .providers
        .apply_snapshot(vec![provider_summary("probefix")], 3);
    model.screen = Screen::Providers;

    model.handle(key(KeyCode::Char('e')));
    model.requests.clear();

    // Focus the origin line and repoint the endpoint to a new address.
    model.handle(key(KeyCode::Tab));
    assert_eq!(
        model.custom_add.as_ref().expect("card").focus,
        CustomField::Origin
    );
    for _ in 0.."http://127.0.0.1:9999/v1".len() {
        model.handle(key(KeyCode::Backspace));
    }
    for c in "http://10.0.0.5:1234/v1".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    assert_eq!(
        model.custom_add.as_ref().expect("card").origin,
        "http://10.0.0.5:1234/v1",
        "the origin line accepts edits in edit mode"
    );

    model.handle(key(KeyCode::Enter));
    let (name, origin, expected_revision) = model
        .requests
        .iter()
        .find_map(|request| match request {
            AppRequest::ProviderConfigure {
                name,
                origin,
                expected_revision,
                ..
            } => Some((name.clone(), origin.clone(), *expected_revision)),
            _ => None,
        })
        .expect("edit submit emits a provider.configure");
    // The NAME (stable identity) is unchanged; the ORIGIN carries the
    // repoint; the observed revision gates the durable write.
    assert_eq!(name, "probefix");
    assert_eq!(origin, "http://10.0.0.5:1234/v1");
    assert_eq!(expected_revision, 3);
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

/// MUTATION CHECK: drop the `push_custom_card_lines` call from the
/// PROVIDERS renderer. Expected RUNTIME failure: the card opened from
/// /providers is invisible (the live-probe bug this pins).
#[test]
fn cards_opened_from_providers_render_on_the_providers_screen() {
    use haider_tui::render::render;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let mut model = live_model();
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_PROVIDER_CONFIGURE_V1.to_owned());
    model.providers.apply_snapshot(Vec::new(), 1);
    model.screen = Screen::Providers;
    model.handle(key(KeyCode::Char('h')));
    assert!(model.custom_add.is_some());

    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| {
            render(&model, frame);
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
        text.contains("router.huggingface.co"),
        "the HF preset card is visible on /providers"
    );
}

/// MUTATION CHECK: route a `provider.models_refresh` failure back to the
/// generic flash (drop the models context tag). Expected RUNTIME failure:
/// the flash assertion below (owner bug: boot-time auto-refresh of a dead
/// probe provider flashed `provider_error` at the launcher).
#[test]
fn models_refresh_failure_lands_on_the_provider_row_never_the_flash() {
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    model
        .providers
        .apply_snapshot(vec![provider_summary("probefix")], 3);
    model.flash = None;
    pass(
        &mut driver,
        &mut model,
        Some(LiveReply::ModelsRefreshFailed {
            provider: "probefix".into(),
            message: "provider does not expose a subscription model catalog".into(),
        }),
    );
    assert!(
        model.flash.is_none(),
        "no status-bar flash: {:?}",
        model.flash
    );
    let row = model
        .providers
        .providers
        .iter()
        .find(|summary| summary.provider == "probefix")
        .expect("row");
    assert_eq!(
        row.availability,
        haider_rpc::ProviderAvailabilityWire::Unavailable
    );
    assert!(
        row.availability_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("subscription model catalog")),
        "the reason lands on the ROW"
    );
}

/// MUTATION CHECK: drop the providers-screen button row or the
/// Providers-screen `Hit::AccountAdd` arm. Expected RUNTIME failure: the
/// render or the jump-and-open assertion below (owner ask: /providers
/// offers the same add options as /accounts).
#[test]
fn providers_screen_offers_account_add_and_jumps_to_accounts() {
    use haider_tui::app::Hit;
    use haider_tui::render::render;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let mut model = live_model();
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_ACCOUNT_OAUTH_PKCE_V1.to_owned());
    model.providers.apply_snapshot(Vec::new(), 1);
    model.screen = Screen::Providers;

    let backend = TestBackend::new(130, 45);
    let mut terminal = Terminal::new(backend).expect("terminal");
    let mut hits = Vec::new();
    terminal
        .draw(|frame| {
            hits = render(&model, frame);
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
        text.contains("+ Anthropic (OAuth)") && text.contains("+ HuggingFace"),
        "the add buttons render on /providers"
    );
    assert!(
        hits.iter()
            .any(|(_, hit)| matches!(hit, Hit::AccountAdd(_))),
        "the buttons carry hit regions"
    );

    model.handle_hit(Hit::AccountAdd(
        haider_tui::app::AccountAddKind::OpenAiOAuth,
    ));
    assert_eq!(model.screen, Screen::Accounts, "the add flow jumps home");
    assert!(model.oauth_add.is_some(), "and opens the card");
}

/// MUTATION CHECK: mint a neutral daemon callsign (`SUB-hex`) or drop the
/// TUI roster claim. Expected RUNTIME failure: the chip wears no
/// honor-roll name (owner ask: subagent names match the sim roster).
#[test]
fn live_chips_claim_roster_callsigns_when_the_wire_sends_none() {
    let mut chips = Vec::new();
    haider_tui::session::apply_agent_payload(
        &mut chips,
        &EventPayload::AgentSpawned(haider_protocol::agent::AgentManifest {
            agent: haider_protocol::ids::AgentId::new("agent-roster-1"),
            role: haider_protocol::agent::AgentRole::Subagent,
            task: "first task".into(),
            callsign: None,
            model_profile: "gpt-5.6-sol".into(),
            grant: haider_protocol::agent::Grant {
                tools: Vec::new(),
                effect_ceiling: Vec::new(),
            },
            budget_tokens: None,
            placement: haider_protocol::agent::Placement::Local,
            lease: haider_protocol::ids::LeaseId::new("lease-roster-1"),
            fencing_epoch: 0,
            attempt: 0,
            parent: None,
            coordinates: None,
        }),
        0,
    );
    let chip = chips.first().expect("chip created");
    assert!(!chip.callsign.is_empty(), "a roster callsign was claimed");
    assert!(
        !chip.callsign.starts_with("SUB-"),
        "never the neutral hex: {}",
        chip.callsign
    );
    assert!(!chip.hon.is_empty(), "the honorific pairs with the claim");
}

/// LAW (owner 2026-08-17, mid-backoff manual retry): while the run is
/// RETRYING (auto-backoff), `/retry` issues the same durable `run.retry`
/// — the daemon's wake seam fires the next attempt NOW — instead of the
/// idle refusal flash.
///
/// MUTATION CHECK: restore the errored-only gate in `issue_run_retry`.
/// Expected RUNTIME failure: mid-backoff `/retry` flashes "did not fail"
/// and no RunRetry request is issued below.
#[test]
fn mid_backoff_retry_issues_the_wake_command() {
    use common::run_slash;
    use haider_protocol::ids::SessionId;
    use haider_protocol::state::RunState;

    let mut model = live_model();
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_RUN_RETRY_V1.to_owned());
    model.upsert_live_session(&SessionId::new("s-backoff"));
    model.open_session(&SessionId::new("s-backoff"));
    model.handle(AppEvent::Envelope(Box::new(EventPayload::RunState(
        RunState::Retrying {
            attempt: 3,
            max: 10,
            delay_ms: 8_000,
            reason: haider_protocol::state::WaitReason::Other {
                tag: "ECONNRESET".to_owned(),
            },
        },
    ))));
    model.requests.clear();
    run_slash(&mut model, "/retry");
    assert!(
        matches!(
            model.requests.last(),
            Some(AppRequest::RunRetry { session }) if session.as_str() == "s-backoff"
        ),
        "mid-backoff /retry sends the wake: {:?}",
        model.requests
    );
}
