#![allow(clippy::expect_used)]

//! W5g-1 — the identity's context window comes from the DISCOVERED
//! catalog (real limits, never guessed).
//!
//! Why: the meter divided real token counts by a hardcoded 200k seed, so
//! every percentage shown for a subscription model was a fiction. The
//! codex catalog declares real windows per model; a declared window always
//! wins, and with none declared the seed stands — an honest fallback, not
//! a fabrication.

use haider_protocol::credential::{AuthMethod, CredentialDescriptor, CredentialStatus};
use haider_protocol::ids::CredentialAlias;
use haider_tui::app::{AppModel, RuntimeMode};
use haider_tui::live::{LiveDriver, LiveReply};
use haider_tui::runtime::live_pass;

mod common;
use common::{launcher_model, run_slash};

fn oauth_descriptor(provider: &str, alias: &str) -> CredentialDescriptor {
    CredentialDescriptor {
        alias: CredentialAlias::new(alias),
        provider: provider.to_owned(),
        base_url: None,
        auth_method: AuthMethod::OAuth,
        identity: "owner@example.test".to_owned(),
        status: CredentialStatus::Ok,
        active: true,
        label: None,
        account_identity: None,
        created_at_ms: None,
    }
}

fn windowed_summary(
    provider: &str,
    entries: &[(&str, Option<u64>)],
    default: &str,
) -> haider_rpc::ProviderSummaryWire {
    haider_rpc::ProviderSummaryWire {
        provider: provider.to_owned(),
        api_family: haider_rpc::ProviderApiFamilyWire::OpenAiResponses,
        endpoint: None,
        response_open_timeout_ms: None,
        chunk_idle_timeout_ms: None,
        semantic_progress_timeout_ms: None,
        models: entries.iter().map(|(slug, _)| (*slug).to_owned()).collect(),
        model_details: entries
            .iter()
            .map(|(slug, window)| haider_rpc::ModelDetailWire {
                name: (*slug).to_owned(),
                context_window: *window,
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

/// MUTATION CHECK (W5g-1): empty `refresh_context_window`'s body (make it
/// a no-op). Expected runtime failure: the identity below keeps the 200k
/// seed instead of the provider's declared 272k.
#[test]
fn adoption_takes_the_provider_declared_window() {
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    let seed = model.identity.context_window;

    pass(
        &mut driver,
        &mut model,
        LiveReply::Accounts {
            descriptors: vec![oauth_descriptor("openai-oauth", "openai-oauth")],
            revision: Some(1),
        },
    );
    pass(
        &mut driver,
        &mut model,
        LiveReply::Providers {
            providers: vec![windowed_summary(
                "openai-oauth",
                &[("gpt-5.6-sol", Some(272_000))],
                "gpt-5.6-sol",
            )],
            revision: 1,
        },
    );

    assert_eq!(model.identity.model_short, "gpt-5.6-sol");
    assert_eq!(
        model.identity.context_window, 272_000,
        "a declared window replaces the seed (seed was {seed})"
    );
}

/// MUTATION CHECK (W5g-1): make `refresh_context_window` substitute a
/// constant (or zero) when the catalog declares no window. Expected
/// runtime failure: the seed below is overwritten by an invented number.
#[test]
fn an_undeclared_window_keeps_the_seed_honest() {
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    let seed = model.identity.context_window;

    pass(
        &mut driver,
        &mut model,
        LiveReply::Accounts {
            descriptors: vec![oauth_descriptor("anthropic-oauth", "anthropic-oauth")],
            revision: Some(1),
        },
    );
    pass(
        &mut driver,
        &mut model,
        LiveReply::Providers {
            providers: vec![windowed_summary(
                "anthropic-oauth",
                &[("claude-opus-5", None)],
                "claude-opus-5",
            )],
            revision: 1,
        },
    );

    assert_eq!(model.identity.model_short, "claude-opus-5");
    assert_eq!(
        model.identity.context_window, seed,
        "no declaration → the current figure stands; never a guess"
    );
}

/// MUTATION CHECK (W5g-1): remove the `refresh_context_window` call from
/// the driver's `ProviderModelsRefreshed` arm. Expected runtime failure:
/// the pinned identity below keeps the seed — the bootstrap returns early
/// on the pin, so no other path can carry the late-arriving real window.
#[test]
fn a_late_catalog_updates_even_a_pinned_identity() {
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    let seed = model.identity.context_window;

    pass(
        &mut driver,
        &mut model,
        LiveReply::Providers {
            providers: vec![windowed_summary(
                "openai-oauth",
                &[("gpt-5.6-sol", None)],
                "gpt-5.6-sol",
            )],
            revision: 1,
        },
    );
    model.identity.provider = "openai-oauth".to_owned();
    // F2a: /model opens the picker pre-filtered; ⏎ selects the row.
    run_slash(&mut model, "/model gpt-5.6-sol");
    model.handle(common::key(ratatui::crossterm::event::KeyCode::Enter));
    assert!(model.identity_pinned, "/model is an explicit choice");
    assert_eq!(model.identity.context_window, seed);

    pass(
        &mut driver,
        &mut model,
        LiveReply::ProviderModelsRefreshed {
            provider: windowed_summary(
                "openai-oauth",
                &[("gpt-5.6-sol", Some(272_000))],
                "gpt-5.6-sol",
            ),
            revision: 2,
        },
    );

    assert_eq!(
        model.identity.context_window, 272_000,
        "the pin protects the user's choice, not a stale number"
    );
    assert_eq!(model.identity.model_short, "gpt-5.6-sol");
    assert!(model.identity_pinned, "the refresh never unpins");
}

/// MUTATION CHECK (W5g-1): remove the `refresh_context_window` call from
/// the `/model` picker arm. Expected runtime failure: the pick below keeps
/// the default model's window — no snapshot follows to correct it.
#[test]
fn the_model_picker_adopts_the_picked_models_window() {
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");

    pass(
        &mut driver,
        &mut model,
        LiveReply::Accounts {
            descriptors: vec![oauth_descriptor("openai-oauth", "openai-oauth")],
            revision: Some(1),
        },
    );
    pass(
        &mut driver,
        &mut model,
        LiveReply::Providers {
            providers: vec![windowed_summary(
                "openai-oauth",
                &[
                    ("gpt-5.6-sol", Some(272_000)),
                    ("gpt-5.3-codex-spark", Some(128_000)),
                ],
                "gpt-5.6-sol",
            )],
            revision: 1,
        },
    );
    assert_eq!(model.identity.context_window, 272_000);

    run_slash(&mut model, "/model gpt-5.3-codex-spark");
    model.handle(common::key(ratatui::crossterm::event::KeyCode::Enter));

    assert_eq!(model.identity.model_short, "gpt-5.3-codex-spark");
    assert_eq!(
        model.identity.context_window, 128_000,
        "the picked model's own declared window applies"
    );
}
