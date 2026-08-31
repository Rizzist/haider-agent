#![allow(clippy::expect_used)]

//! W5f-2 — the composer identity follows DAEMON TRUTH until the user pins
//! a choice, and `session.create` requests an OUTPUT budget, not the
//! context window.
//!
//! Why: the live screenshot chain (v0.0.19) died on both counts — the
//! launcher created sessions on the demo seed pair (a provider with no
//! account, instant ✗ ERRORED), and the create carried `max_tokens =
//! 200_000`, which providers read as the per-request OUTPUT cap and
//! Anthropic rejects outright.

use haider_protocol::credential::{AuthMethod, CredentialDescriptor, CredentialStatus};
use haider_protocol::ids::CredentialAlias;
use haider_tui::app::{AppModel, AppRequest, RuntimeMode};
use haider_tui::live::{
    LiveCommand, LiveDriver, LiveReply, SESSION_OUTPUT_CAP, session_output_cap,
};
use haider_tui::runtime::live_pass;

mod common;
use common::{launcher_model, run_slash};

fn oauth_descriptor(provider: &str, alias: &str, active: bool) -> CredentialDescriptor {
    CredentialDescriptor {
        alias: CredentialAlias::new(alias),
        provider: provider.to_owned(),
        base_url: None,
        auth_method: AuthMethod::OAuth,
        identity: "owner@example.test".to_owned(),
        status: CredentialStatus::Ok,
        active,
        label: None,
        account_identity: None,
        created_at_ms: None,
    }
}

fn provider_summary(
    provider: &str,
    models: &[&str],
    default: &str,
) -> haider_rpc::ProviderSummaryWire {
    haider_rpc::ProviderSummaryWire {
        provider: provider.to_owned(),
        api_family: haider_rpc::ProviderApiFamilyWire::OpenAiResponses,
        endpoint: None,
        response_open_timeout_ms: None,
        chunk_idle_timeout_ms: None,
        semantic_progress_timeout_ms: None,
        models: models.iter().map(|slug| (*slug).to_owned()).collect(),
        model_details: models
            .iter()
            .map(|slug| haider_rpc::ModelDetailWire {
                name: (*slug).to_owned(),
                display_name: None,
                context_window: None,
                supported_efforts: Vec::new(),
                default_effort: None,
                supported_speeds: Vec::new(),
                supports_thinking_type: None,
            })
            .collect(),
        inventory_fetched_at_ms: None,
        inventory_authority: haider_rpc::ModelInventoryAuthorityWire::Unknown,
        auth_methods: vec![AuthMethod::OAuth],
        availability: haider_rpc::ProviderAvailabilityWire::Available,
        availability_reason: None,
        default_model: Some(default.to_owned()),
        enabled: true,
        trust: haider_rpc::ProviderTrustWire::Full,
    }
}

fn live_model() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model
}

fn pass(driver: &mut LiveDriver, model: &mut AppModel, reply: LiveReply) {
    live_pass(driver, model, Some(reply), std::time::Instant::now());
}

/// MUTATION CHECK (W5f-2): remove the `bootstrap_identity_from_daemon`
/// calls from the driver's Accounts AND Providers snapshot arms. Expected
/// runtime failure: the identity below stays on the demo seed pair — the
/// exact live failure (first session pinned to a provider with no
/// account).
/// Verified by revert on 2026-07-30.
#[test]
fn an_unpinned_identity_follows_the_imported_active_account() {
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    let seed_provider = model.identity.provider.clone();

    pass(
        &mut driver,
        &mut model,
        LiveReply::Accounts {
            descriptors: vec![oauth_descriptor("openai-oauth", "openai-oauth", true)],
            revision: Some(1),
        },
    );
    // Account truth ALONE must not adopt: a half-identity (right provider,
    // demo-seed model) would send a foreign slug to the subscription API.
    assert_eq!(
        model.identity.provider, seed_provider,
        "no adoption before the provider's model truth arrives"
    );

    // The provider snapshot completes the picture: NOW the identity adopts
    // the account AND the provider's OWN declared default model.
    pass(
        &mut driver,
        &mut model,
        LiveReply::Providers {
            providers: vec![provider_summary(
                "openai-oauth",
                &["gpt-5.6", "gpt-5.6-codex"],
                "gpt-5.6-codex",
            )],
            revision: 1,
        },
    );
    assert_eq!(model.identity.provider, "openai-oauth");
    assert_eq!(model.identity.account, "openai-oauth");
    assert_eq!(model.identity.model_short, "gpt-5.6-codex");
    assert!(
        !model.identity_pinned,
        "a bootstrap is not a user choice — later daemon truth may still move it"
    );
}

/// MUTATION CHECK (W5f-2): make `bootstrap_identity_from_daemon` ignore
/// `identity_pinned`. Expected runtime failure: the snapshot below
/// overwrites the user's explicit `/model` pick.
/// Verified by revert on 2026-07-30.
#[test]
fn a_pinned_choice_survives_every_later_snapshot() {
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    pass(
        &mut driver,
        &mut model,
        LiveReply::Providers {
            providers: vec![provider_summary(
                "openai-oauth",
                &["gpt-5.6", "o4-mini"],
                "gpt-5.6",
            )],
            revision: 1,
        },
    );
    model.identity.provider = "openai-oauth".to_owned();
    // F2a: /model opens the full-screen picker pre-filtered; ⏎ selects
    // the highlighted discovered row — still an explicit user choice.
    run_slash(&mut model, "/model o4-mini");
    model.handle(common::key(ratatui::crossterm::event::KeyCode::Enter));
    assert_eq!(model.identity.model_short, "o4-mini");
    assert!(model.identity_pinned, "/model is an explicit choice");

    pass(
        &mut driver,
        &mut model,
        LiveReply::Accounts {
            descriptors: vec![oauth_descriptor("anthropic-oauth", "anthropic-oauth", true)],
            revision: Some(2),
        },
    );
    assert_eq!(
        model.identity.provider, "openai-oauth",
        "a pinned identity never follows a later snapshot"
    );
    assert_eq!(model.identity.model_short, "o4-mini");
}

/// MUTATION CHECK (W5f-2): make `apply_account_selected` adopt WITHOUT
/// pinning (pass `false`). Expected runtime failure: the follow-up
/// snapshot below steals the identity the user just clicked.
/// Verified by revert on 2026-07-30.
#[test]
fn clicking_an_account_adopts_it_into_the_identity_and_pins() {
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    pass(
        &mut driver,
        &mut model,
        LiveReply::Providers {
            providers: vec![provider_summary(
                "anthropic-oauth",
                &["claude-opus-5"],
                "claude-opus-5",
            )],
            revision: 1,
        },
    );
    model.apply_account_selected(&oauth_descriptor("anthropic-oauth", "work-max", true), 2);
    assert_eq!(model.identity.provider, "anthropic-oauth");
    assert_eq!(model.identity.account, "work-max");
    assert_eq!(model.identity.model_short, "claude-opus-5");

    pass(
        &mut driver,
        &mut model,
        LiveReply::Accounts {
            descriptors: vec![oauth_descriptor("openai-oauth", "openai-oauth", true)],
            revision: Some(3),
        },
    );
    assert_eq!(
        model.identity.provider, "anthropic-oauth",
        "the clicked account is a pinned choice"
    );
}

/// MUTATION CHECK (W5f-2): pass `model.identity.context_window` raw in the
/// driver's `CreateSession` arm. Expected runtime failure: the command
/// below carries 200_000 — the output budget Anthropic rejects.
/// Verified by revert on 2026-07-30.
#[test]
fn session_create_requests_an_output_budget_not_the_context_window() {
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    model.identity.context_window = 200_000;
    model.requests.push(AppRequest::CreateSession {
        text: "hello".to_owned(),
    });
    let pass = live_pass(&mut driver, &mut model, None, std::time::Instant::now());
    let created = pass
        .commands
        .iter()
        .find_map(|command| match command {
            LiveCommand::Create { max_tokens, .. } => Some(*max_tokens),
            _ => None,
        })
        .expect("create command");
    assert_eq!(created, SESSION_OUTPUT_CAP);

    // A tinier declared window still wins.
    assert_eq!(session_output_cap(4_096), 4_096);
    assert_eq!(
        session_output_cap(0),
        1,
        "zero never reaches the daemon's reject"
    );
}

/// MUTATION CHECK (W5f-2/2c): drop `AccountList`/`ProviderList` from
/// `resume`'s front-door command set, and separately from `boot`'s.
/// Expected runtime failure: the corresponding half below produces
/// neither read, so the bootstrap's snapshots never arrive and the
/// launcher sits on demo seeds — `boot` is EXACTLY what the v0.0.21
/// live probe caught missing (Reconnected only fires on redials).
/// Verified by revert on 2026-07-30.
#[test]
fn connecting_asks_for_account_and_provider_truth() {
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");

    // The FIRST connect (boot) — the half the live probe caught.
    let boot = driver.boot();
    assert!(
        boot.iter()
            .any(|command| matches!(command, LiveCommand::AccountList)),
        "boot must ask for account truth: {boot:?}"
    );
    assert!(
        boot.iter()
            .any(|command| matches!(command, LiveCommand::ProviderList)),
        "and provider truth: {boot:?}"
    );

    // Every redial (resume) keeps the same front-door reads.
    let pass = live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Reconnected),
        std::time::Instant::now(),
    );
    assert!(
        pass.commands
            .iter()
            .any(|command| matches!(command, LiveCommand::AccountList)),
        "a reconnect must ask for account truth: {:?}",
        pass.commands
    );
    assert!(
        pass.commands
            .iter()
            .any(|command| matches!(command, LiveCommand::ProviderList)),
        "and provider truth: {:?}",
        pass.commands
    );
}

/// MUTATION CHECK (W5f-2d): make the driver's Accounts arm return
/// `Vec::new()` instead of `self.provider_model_refreshes(model)`. Expected
/// runtime failure: an active OAuth account with no catalog triggers no
/// discovery, so the picker and bootstrap stay empty forever — the exact
/// live symptom before this fix (identity stuck on the demo seed).
/// Verified by revert on 2026-07-30.
#[test]
fn an_active_oauth_account_with_no_catalog_triggers_discovery() {
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    let pass = live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Accounts {
            descriptors: vec![oauth_descriptor("openai-oauth", "openai-oauth", true)],
            revision: Some(1),
        }),
        std::time::Instant::now(),
    );
    assert!(
        pass.commands.iter().any(|command| matches!(
            command,
            LiveCommand::RefreshProviderModels { provider } if provider == "openai-oauth"
        )),
        "the active OAuth provider must have its catalog discovered: {:?}",
        pass.commands
    );

    // ONE request per provider per connection — a second snapshot does not
    // re-ask.
    let again = live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Accounts {
            descriptors: vec![oauth_descriptor("openai-oauth", "openai-oauth", true)],
            revision: Some(2),
        }),
        std::time::Instant::now(),
    );
    assert!(
        !again
            .commands
            .iter()
            .any(|command| matches!(command, LiveCommand::RefreshProviderModels { .. })),
        "discovery must not storm on every snapshot: {:?}",
        again.commands
    );
}

/// The refreshed catalog lands via `ProviderModelsRefreshed` and completes
/// the bootstrap to the provider's real default model.
#[test]
fn a_refreshed_catalog_completes_the_bootstrap() {
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    let seed_provider = model.identity.provider.clone();
    // Account active, provider present but WITHOUT models yet.
    live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Accounts {
            descriptors: vec![oauth_descriptor("openai-oauth", "openai-oauth", true)],
            revision: Some(1),
        }),
        std::time::Instant::now(),
    );
    assert_eq!(
        model.identity.provider, seed_provider,
        "no adoption before the catalog arrives"
    );
    // The catalog arrives.
    pass(
        &mut driver,
        &mut model,
        LiveReply::ProviderModelsRefreshed {
            provider: provider_summary(
                "openai-oauth",
                &["gpt-5.6-sol", "gpt-5.6-terra"],
                "gpt-5.6-sol",
            ),
            revision: 2,
        },
    );
    assert_eq!(model.identity.provider, "openai-oauth");
    assert_eq!(model.identity.model_short, "gpt-5.6-sol");
}
