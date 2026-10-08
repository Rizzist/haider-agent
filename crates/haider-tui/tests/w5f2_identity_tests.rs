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
use haider_tui::link::command_required_features;
use haider_tui::live::{LiveCommand, LiveDriver, LiveReply};
use haider_tui::runtime::live_pass;

mod common;
use common::{launcher_model, run_slash};

fn oauth_descriptor(provider: &str, alias: &str, active: bool) -> CredentialDescriptor {
    CredentialDescriptor {
        incarnation: 0,
        credential_digest: None,
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
        catalog_scope: None,
        configured_models: Vec::new(),
        static_models: Vec::new(),
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
                source: None,
                name: (*slug).to_owned(),
                display_name: None,
                context_window: None,
                max_output_tokens: Some(30_000),
                supported_efforts: Vec::new(),
                default_effort: None,
                supported_speeds: Vec::new(),
                supports_thinking_type: None,
                supports_vision: None,
            })
            .collect(),
        catalog: haider_rpc::ProviderCatalogKindWire::Unknown,
        inventory: haider_rpc::ModelInventoryWire::Static,
        inventory_authority: haider_rpc::ModelInventoryAuthorityWire::Unknown,
        auth_methods: vec![AuthMethod::OAuth],
        availability: haider_rpc::ProviderAvailabilityWire::Available,
        availability_reason: None,
        default_model: Some(default.to_owned()),
        enabled: true,
        trust: haider_rpc::ProviderTrustWire::Full,
    }
}

fn private_summary(incarnation: u64, model: &str) -> haider_rpc::ProviderSummaryWire {
    let mut summary = provider_summary("custom", &[model], model);
    summary.catalog_scope = Some(haider_rpc::ProviderCatalogScopeWire {
        key: format!("synthetic-producer-{incarnation}"),
        account_alias: Some("work".into()),
        account_incarnation: Some(incarnation),
    });
    summary.inventory = haider_rpc::ModelInventoryWire::Fetched { fetched_at_ms: 1 };
    summary.inventory_authority = haider_rpc::ModelInventoryAuthorityWire::Advisory;
    summary.model_details[0].source = Some(haider_rpc::ModelDetailSourceWire::Remote);
    summary.model_details[0].context_window = Some(123_456);
    summary
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
            sources: Vec::new(),
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

#[test]
fn fresh_oauth_login_adopts_static_model_after_catalog_403() {
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    let seed_provider = model.identity.provider.clone();

    pass(
        &mut driver,
        &mut model,
        LiveReply::Accounts {
            descriptors: vec![oauth_descriptor("anthropic-oauth", "work", true)],
            revision: Some(1),
            sources: Vec::new(),
        },
    );
    assert_eq!(model.identity.provider, seed_provider);

    let mut summary = provider_summary(
        "anthropic-oauth",
        &["claude-fable-5-1", "claude-sonnet-4-5"],
        "claude-fable-5-1",
    );
    summary.inventory = haider_rpc::ModelInventoryWire::Unavailable {
        reason: "GET /v1/models returned (403)".to_owned(),
    };
    for detail in &mut summary.model_details {
        detail.source = Some(haider_rpc::ModelDetailSourceWire::Static);
    }
    pass(
        &mut driver,
        &mut model,
        LiveReply::Providers {
            providers: vec![summary],
            revision: 1,
        },
    );

    assert_eq!(model.identity.provider, "anthropic-oauth");
    assert_eq!(model.identity.account, "work");
    assert_eq!(model.identity.model_short, "claude-fable-5-1");
    assert!(!model.identity_pinned);
    model.open_model_picker(String::new());
    assert!(model.model_picker_rows().iter().any(|row| {
        row.provider == "anthropic-oauth" && row.model == "claude-fable-5-1" && row.selectable
    }));
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
            sources: Vec::new(),
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
            sources: Vec::new(),
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
    let create = pass
        .commands
        .iter()
        .find(|command| matches!(command, LiveCommand::Create { .. }))
        .expect("create command");
    let LiveCommand::Create {
        max_tokens: created,
        ..
    } = create
    else {
        unreachable!()
    };
    assert_eq!(
        *created, 0,
        "the daemon derives the selected model's output budget"
    );
    assert!(
        command_required_features(create).contains(&haider_rpc::FEATURE_MODEL_OUTPUT_LIMITS_V1)
    );
}

#[test]
fn session_create_before_model_details_requests_daemon_derivation() {
    let mut model = live_model();
    let mut driver = LiveDriver::new("unknown-detail");
    model.identity.provider = "gemini".into();
    model.identity.model_short = "gemini-2.0-flash".into();
    model.identity.context_window = 0;
    model.requests.push(AppRequest::CreateSession {
        text: "hello".into(),
    });
    let pass = live_pass(&mut driver, &mut model, None, std::time::Instant::now());
    assert!(
        pass.commands
            .iter()
            .any(|command| matches!(command, LiveCommand::Create { max_tokens: 0, .. }))
    );
    assert!(
        haider_tui::render::status_left_segments(&model, 120)
            .iter()
            .any(|segment| segment.text.contains("window unknown"))
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
            sources: Vec::new(),
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
            sources: Vec::new(),
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
            sources: Vec::new(),
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

#[test]
fn never_fetched_state_refreshes_even_when_legacy_rows_are_nonempty() {
    let mut model = live_model();
    let mut summary = provider_summary("openai-oauth", &["legacy-seed"], "legacy-seed");
    summary.inventory = haider_rpc::ModelInventoryWire::NeverFetched;
    model.providers.apply_snapshot(vec![summary], 1);
    let before = model.identity.model_short.clone();
    let mut driver = LiveDriver::new("test");
    let pass = live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Accounts {
            descriptors: vec![oauth_descriptor("openai-oauth", "openai-oauth", true)],
            revision: Some(1),
            sources: Vec::new(),
        }),
        std::time::Instant::now(),
    );
    assert!(pass.commands.iter().any(|command| matches!(command, LiveCommand::RefreshProviderModels { provider } if provider == "openai-oauth")));
    assert_eq!(
        model.identity.model_short, before,
        "never adopt a legacy seed before discovery"
    );
}

#[test]
fn same_alias_new_incarnation_retires_current_picker_and_launcher_default() {
    for account_first in [true, false] {
        let mut model = live_model();
        let mut driver = LiveDriver::new("test");
        let mut a = oauth_descriptor("custom", "work", true);
        a.incarnation = 1;
        let mut summary = provider_summary("custom", &["a-private"], "a-private");
        summary.inventory_authority = haider_rpc::ModelInventoryAuthorityWire::Advisory;
        summary.model_details[0].source = Some(haider_rpc::ModelDetailSourceWire::Remote);
        pass(
            &mut driver,
            &mut model,
            LiveReply::Accounts {
                descriptors: vec![a.clone()],
                revision: Some(1),
                sources: vec![],
            },
        );
        pass(
            &mut driver,
            &mut model,
            LiveReply::Providers {
                providers: vec![summary.clone()],
                revision: 1,
            },
        );
        assert_eq!(model.identity.model_short, "a-private");
        model.open_model_picker("a-private".into());
        assert!(model.requests.contains(&AppRequest::AccountsRefresh));
        assert!(model.requests.contains(&AppRequest::ProvidersRefresh));
        model.apply_model_selected("custom", "a-private");
        // Benign refresh keeps both the choice and its pin.
        pass(
            &mut driver,
            &mut model,
            LiveReply::Accounts {
                descriptors: vec![a.clone()],
                revision: Some(1),
                sources: vec![],
            },
        );
        assert_eq!(model.identity.model_short, "a-private");
        assert!(model.identity_pinned);
        let mut unrelated = oauth_descriptor("unrelated", "other", true);
        unrelated.incarnation = 3;
        let mut unrelated_summary =
            provider_summary("unrelated", &["other-current"], "other-current");
        unrelated_summary.model_details[0].source = Some(haider_rpc::ModelDetailSourceWire::Static);
        let mut b = a;
        b.incarnation = 2;
        summary.models = vec!["b-current".into()];
        summary.model_details[0].name = "b-current".into();
        summary.default_model = Some("b-current".into());
        if !account_first {
            pass(
                &mut driver,
                &mut model,
                LiveReply::Providers {
                    providers: vec![unrelated_summary.clone(), summary.clone()],
                    revision: 2,
                },
            );
        }
        pass(
            &mut driver,
            &mut model,
            LiveReply::Accounts {
                descriptors: vec![unrelated, b],
                revision: Some(2),
                sources: vec![],
            },
        );
        if account_first {
            assert!(
                model.identity.model_short.is_empty(),
                "old provider snapshot cannot restore A's model"
            );
            assert!(
                !model
                    .model_picker_rows()
                    .iter()
                    .any(|row| row.model == "a-private"),
                "account reply fences the old provider reply immediately"
            );
            pass(
                &mut driver,
                &mut model,
                LiveReply::Providers {
                    providers: vec![unrelated_summary.clone(), summary.clone()],
                    revision: 2,
                },
            );
        }
        assert_eq!(model.identity.provider, "custom");
        assert_eq!(model.identity.model_short, "b-current");
        pass(
            &mut driver,
            &mut model,
            LiveReply::Providers {
                providers: vec![unrelated_summary, summary],
                revision: 2,
            },
        );
        assert_eq!(model.identity.provider, "custom");
        assert_eq!(model.identity.model_short, "b-current");
        assert!(
            !model
                .model_picker_rows()
                .iter()
                .any(|row| row.model == "a-private")
        );
    }
}

#[test]
fn static_or_configured_picker_choice_survives_an_account_incarnation_change() {
    for source in [
        haider_rpc::ModelDetailSourceWire::Static,
        haider_rpc::ModelDetailSourceWire::Configured,
    ] {
        let mut model = live_model();
        let mut driver = LiveDriver::new("test");
        let mut descriptor = oauth_descriptor("custom", "work", true);
        descriptor.incarnation = 1;
        let mut summary = provider_summary(
            "custom",
            &["permanent-choice", "other-default"],
            "permanent-choice",
        );
        for detail in &mut summary.model_details {
            detail.source = Some(source);
        }
        pass(
            &mut driver,
            &mut model,
            LiveReply::Accounts {
                descriptors: vec![descriptor.clone()],
                revision: Some(1),
                sources: vec![],
            },
        );
        pass(
            &mut driver,
            &mut model,
            LiveReply::Providers {
                providers: vec![summary.clone()],
                revision: 1,
            },
        );
        model.apply_model_selected("custom", "permanent-choice");
        descriptor.incarnation = 2;
        pass(
            &mut driver,
            &mut model,
            LiveReply::Accounts {
                descriptors: vec![descriptor],
                revision: Some(2),
                sources: vec![],
            },
        );
        summary.default_model = Some("other-default".into());
        pass(
            &mut driver,
            &mut model,
            LiveReply::Providers {
                providers: vec![summary],
                revision: 2,
            },
        );
        assert_eq!(model.identity.model_short, "permanent-choice");
        assert!(model.identity_pinned);
    }
}

#[test]
fn private_choice_before_first_accounts_reply_is_not_stamped_as_replacement_owner() {
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    pass(
        &mut driver,
        &mut model,
        LiveReply::Providers {
            providers: vec![private_summary(1, "a-private")],
            revision: 1,
        },
    );
    model.apply_model_selected("custom", "a-private");
    let mut b = oauth_descriptor("custom", "work", true);
    b.incarnation = 2;
    pass(
        &mut driver,
        &mut model,
        LiveReply::Accounts {
            descriptors: vec![b],
            revision: Some(2),
            sources: vec![],
        },
    );
    assert!(model.identity.model_short.is_empty());
    assert_eq!(model.identity.context_window, 0);
    assert_eq!(model.context_meter().window, None);
    assert!(
        !model
            .model_picker_rows()
            .iter()
            .any(|row| row.model == "a-private")
    );
    pass(
        &mut driver,
        &mut model,
        LiveReply::Providers {
            providers: vec![private_summary(2, "b-current")],
            revision: 2,
        },
    );
    assert_eq!(model.identity.model_short, "b-current");
}

#[test]
fn account_selection_reply_cannot_pin_the_old_provider_default() {
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    let mut a = oauth_descriptor("custom", "work", true);
    a.incarnation = 1;
    pass(
        &mut driver,
        &mut model,
        LiveReply::Accounts {
            descriptors: vec![a.clone()],
            revision: Some(1),
            sources: vec![],
        },
    );
    pass(
        &mut driver,
        &mut model,
        LiveReply::Providers {
            providers: vec![private_summary(1, "a-private")],
            revision: 1,
        },
    );
    model.apply_model_selected("custom", "a-private");
    a.incarnation = 2;
    model.apply_account_selected(&a, 2);
    assert!(model.identity.model_short.is_empty());
    assert!(!model.identity_pinned);
    pass(
        &mut driver,
        &mut model,
        LiveReply::Providers {
            providers: vec![private_summary(2, "b-current")],
            revision: 2,
        },
    );
    assert_eq!(model.identity.model_short, "b-current");
    assert!(
        !model
            .model_picker_rows()
            .iter()
            .any(|row| row.model == "a-private")
    );
}

#[test]
fn unrelated_provider_delta_does_not_make_a_retired_catalog_current() {
    for provider_first in [false, true] {
        let mut model = live_model();
        let mut driver = LiveDriver::new("test");
        let mut account = oauth_descriptor("custom", "work", true);
        account.incarnation = 1;
        pass(
            &mut driver,
            &mut model,
            LiveReply::Accounts {
                descriptors: vec![account.clone()],
                revision: Some(1),
                sources: vec![],
            },
        );
        pass(
            &mut driver,
            &mut model,
            LiveReply::Providers {
                providers: vec![private_summary(1, "a-private")],
                revision: 1,
            },
        );
        model.apply_model_selected("custom", "a-private");
        pass(
            &mut driver,
            &mut model,
            LiveReply::ProviderModelsRefreshed {
                provider: provider_summary("unrelated", &["public"], "public"),
                revision: 2,
            },
        );
        if provider_first {
            pass(
                &mut driver,
                &mut model,
                LiveReply::ProviderModelsRefreshed {
                    provider: private_summary(2, "b-current"),
                    revision: 2,
                },
            );
            assert!(
                !model
                    .model_picker_rows()
                    .iter()
                    .any(|row| row.model == "a-private")
            );
            // Another delta while B waits must not replace B's raw catalog
            // with the fenced projection shown for A.
            pass(
                &mut driver,
                &mut model,
                LiveReply::ProviderModelsRefreshed {
                    provider: provider_summary("unrelated", &["public"], "public"),
                    revision: 2,
                },
            );
        }
        account.incarnation = 2;
        pass(
            &mut driver,
            &mut model,
            LiveReply::Accounts {
                descriptors: vec![account],
                revision: Some(2),
                sources: vec![],
            },
        );
        assert!(
            !model
                .model_picker_rows()
                .iter()
                .any(|row| row.model == "a-private")
        );
        if !provider_first {
            assert!(model.identity.model_short.is_empty());
            pass(
                &mut driver,
                &mut model,
                LiveReply::ProviderModelsRefreshed {
                    provider: private_summary(2, "b-current"),
                    revision: 2,
                },
            );
        }
        assert_eq!(model.identity.model_short, "b-current");
        assert!(
            model
                .model_picker_rows()
                .iter()
                .any(|row| row.model == "b-current" && row.is_current)
        );
    }
}

#[test]
fn public_fetched_choice_and_same_producer_refresh_keep_the_user_pin() {
    for public in [true, false] {
        let mut model = live_model();
        let mut driver = LiveDriver::new("test");
        let mut account = oauth_descriptor("custom", "work", true);
        account.incarnation = 1;
        let mut summary = private_summary(1, "stable-choice");
        if public {
            summary.catalog = haider_rpc::ProviderCatalogKindWire::Public;
            summary.catalog_scope = None;
        }
        pass(
            &mut driver,
            &mut model,
            LiveReply::Accounts {
                descriptors: vec![account.clone()],
                revision: Some(1),
                sources: vec![],
            },
        );
        pass(
            &mut driver,
            &mut model,
            LiveReply::Providers {
                providers: vec![summary.clone()],
                revision: 1,
            },
        );
        model.apply_model_selected("custom", "stable-choice");
        if public {
            account.incarnation = 2;
        }
        pass(
            &mut driver,
            &mut model,
            LiveReply::Accounts {
                descriptors: vec![account],
                revision: Some(2),
                sources: vec![],
            },
        );
        pass(
            &mut driver,
            &mut model,
            LiveReply::ProviderModelsRefreshed {
                provider: summary,
                revision: 2,
            },
        );
        assert_eq!(model.identity.model_short, "stable-choice");
        assert!(model.identity_pinned);
        assert_eq!(model.identity.context_window, 123_456);
    }
}

#[test]
fn remote_metadata_for_a_static_slug_is_retired_without_unpinning_the_static_choice() {
    let provider = "kimi-oauth";
    let slug = haider_provider::subscription_static_models(provider)
        .first()
        .expect("maintained Kimi model")
        .slug
        .clone();
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    let mut account = oauth_descriptor(provider, "work", true);
    account.incarnation = 1;
    let mut summary = private_summary(1, &slug);
    summary.provider = provider.into();
    summary.catalog = haider_rpc::ProviderCatalogKindWire::Authenticated;
    summary.static_models = vec![slug.clone()];
    summary.model_details[0].supports_vision = Some(false);
    pass(
        &mut driver,
        &mut model,
        LiveReply::Accounts {
            descriptors: vec![account.clone()],
            revision: Some(1),
            sources: vec![],
        },
    );
    pass(
        &mut driver,
        &mut model,
        LiveReply::Providers {
            providers: vec![summary],
            revision: 1,
        },
    );
    model.apply_model_selected(provider, &slug);
    assert_eq!(model.identity.context_window, 123_456);
    account.incarnation = 2;
    pass(
        &mut driver,
        &mut model,
        LiveReply::Accounts {
            descriptors: vec![account],
            revision: Some(2),
            sources: vec![],
        },
    );
    assert_eq!(model.identity.model_short, slug);
    assert!(model.identity_pinned);
    assert_eq!(model.identity.context_window, 0, "A's context is retired");
    let detail = model
        .providers
        .providers
        .iter()
        .flat_map(|row| &row.model_details)
        .find(|detail| detail.name == slug)
        .expect("release-owned name remains");
    assert_eq!(
        detail.source,
        Some(haider_rpc::ModelDetailSourceWire::Static)
    );
    assert_eq!(
        detail.supports_vision, None,
        "A's capability flags are retired"
    );
    assert_eq!(detail.context_window, None);
}

#[test]
fn unsupported_configured_deployment_evidence_is_not_inherited_by_a_replacement() {
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    let mut account = oauth_descriptor("custom", "work", true);
    account.incarnation = 1;
    let mut summary = private_summary(1, "my-deployment");
    summary.inventory = haider_rpc::ModelInventoryWire::Configured;
    summary.model_details[0].source = Some(haider_rpc::ModelDetailSourceWire::Configured);
    summary.availability_reason =
        Some("configured (unverified): server returned 404 for /models".into());
    pass(
        &mut driver,
        &mut model,
        LiveReply::Accounts {
            descriptors: vec![account.clone()],
            revision: Some(1),
            sources: vec![],
        },
    );
    pass(
        &mut driver,
        &mut model,
        LiveReply::Providers {
            providers: vec![summary],
            revision: 1,
        },
    );
    model.apply_model_selected("custom", "my-deployment");
    account.incarnation = 2;
    pass(
        &mut driver,
        &mut model,
        LiveReply::Accounts {
            descriptors: vec![account],
            revision: Some(2),
            sources: vec![],
        },
    );
    assert_eq!(
        model.identity.model_short, "my-deployment",
        "explicit deployment survives"
    );
    let row = model
        .model_picker_rows()
        .into_iter()
        .find(|row| row.model == "my-deployment")
        .expect("configured row remains visible");
    assert!(
        !row.available,
        "A's unsupported-listing permission is retired"
    );
    assert!(!row.reason.expect("fresh fetch reason").contains("404"));
}

#[test]
fn anonymous_catalog_is_not_owned_by_the_account_row_waiting_to_be_removed() {
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    let mut account = oauth_descriptor("custom", "work", true);
    account.incarnation = 1;
    pass(
        &mut driver,
        &mut model,
        LiveReply::Accounts {
            descriptors: vec![account],
            revision: Some(1),
            sources: vec![],
        },
    );
    pass(
        &mut driver,
        &mut model,
        LiveReply::Providers {
            providers: vec![private_summary(1, "a-private")],
            revision: 1,
        },
    );
    model.apply_model_selected("custom", "a-private");
    let mut anonymous = private_summary(2, "anonymous-current");
    anonymous.auth_methods.clear();
    anonymous.catalog_scope = Some(haider_rpc::ProviderCatalogScopeWire {
        key: "synthetic-anonymous-origin".into(),
        account_alias: None,
        account_incarnation: None,
    });
    pass(
        &mut driver,
        &mut model,
        LiveReply::Providers {
            providers: vec![anonymous],
            revision: 2,
        },
    );
    assert_eq!(model.identity.model_short, "anonymous-current");
    assert!(model.identity.account.is_empty());
    pass(
        &mut driver,
        &mut model,
        LiveReply::Accounts {
            descriptors: vec![],
            revision: Some(2),
            sources: vec![],
        },
    );
    assert_eq!(model.identity.model_short, "anonymous-current");
    assert!(model.identity.account.is_empty());
    assert!(
        !model
            .model_picker_rows()
            .iter()
            .any(|row| row.model == "a-private")
    );
}

#[test]
fn automatic_session_default_is_resolved_by_daemon_instead_of_echoed() {
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    pass(
        &mut driver,
        &mut model,
        LiveReply::Providers {
            providers: vec![private_summary(41, "a-private-alpha")],
            revision: 1,
        },
    );
    model.identity.provider = "custom".into();
    model.identity.model_short = "a-private-alpha".into();
    model.identity_pinned = false;
    model.requests.push(AppRequest::CreateSession {
        text: "hello".into(),
    });
    let pass = live_pass(&mut driver, &mut model, None, std::time::Instant::now());
    let create = pass
        .commands
        .into_iter()
        .find(|command| matches!(command, LiveCommand::Create { .. }))
        .expect("create");
    let haider_rpc::RequestBody::SessionCreateWithPermissionOverrides {
        model,
        resolve_model,
        expected_catalog_key,
        ..
    } = haider_tui::link::request_body(create)
    else {
        unreachable!()
    };
    assert!(
        model.is_empty(),
        "A's derived default never becomes explicit configuration"
    );
    assert!(resolve_model);
    assert_eq!(expected_catalog_key, None);
}

#[test]
fn delayed_session_selection_does_not_borrow_replacement_catalog_owner() {
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    pass(
        &mut driver,
        &mut model,
        LiveReply::Providers {
            providers: vec![private_summary(42, "b-private-beta")],
            revision: 2,
        },
    );
    let mut descriptor = oauth_descriptor("custom", "work", true);
    descriptor.incarnation = 42;
    pass(
        &mut driver,
        &mut model,
        LiveReply::Accounts {
            descriptors: vec![descriptor],
            revision: Some(2),
            sources: Vec::new(),
        },
    );
    let session = haider_protocol::ids::SessionId::new("old-derived-session");
    model.upsert_live_session(&session);
    model.open_session(&session);
    model.apply_scoped_model_selected(
        "custom",
        "a-private-alpha",
        Some("synthetic-producer-41".into()),
    );
    assert!(
        model.identity.model_short.is_empty(),
        "retired current marker removed"
    );
    assert!(
        model.identity_pinned,
        "attached session awaits an explicit choice"
    );
    assert_eq!(model.identity.context_window, 0);
    model.apply_scoped_model_selected(
        "custom",
        "b-private-beta",
        Some("synthetic-producer-42".into()),
    );
    assert_eq!(model.identity.model_short, "b-private-beta");
    assert_eq!(model.identity.context_window, 123_456);
}

#[test]
fn picker_request_captures_owner_before_a_replacement_reply_is_applied() {
    let mut model = live_model();
    model
        .daemon_features
        .insert("session_model_select_v1".into());
    let mut driver = LiveDriver::new("test");
    let mut account = oauth_descriptor("custom", "work", true);
    account.incarnation = 51;
    pass(
        &mut driver,
        &mut model,
        LiveReply::Accounts {
            descriptors: vec![account.clone()],
            revision: Some(1),
            sources: vec![],
        },
    );
    pass(
        &mut driver,
        &mut model,
        LiveReply::Providers {
            providers: vec![private_summary(51, "a-private-alpha")],
            revision: 1,
        },
    );
    let session = haider_protocol::ids::SessionId::new("picker-account-race");
    model.upsert_live_session(&session);
    model.open_session(&session);
    let row = model
        .model_picker_rows()
        .into_iter()
        .find(|row| row.model == "a-private-alpha")
        .expect("A row");
    model.select_model_row(&row);
    assert!(model.requests.iter().any(|request| matches!(request,
        AppRequest::SelectModel { expected_catalog_key: Some(key), .. } if key == "synthetic-producer-51")));
    // A replacement reply reaches the reducer before the queued UI request
    // is drained. Encoding must retain A's observation, not label it B.
    account.incarnation = 52;
    let pass = live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Accounts {
            descriptors: vec![account],
            revision: Some(2),
            sources: vec![],
        }),
        std::time::Instant::now(),
    );
    let select = pass
        .commands
        .into_iter()
        .find(|command| matches!(command, LiveCommand::SelectModel { .. }))
        .expect("queued selection");
    let haider_rpc::RequestBody::SessionSelectModel {
        model,
        expected_catalog_key,
        ..
    } = haider_tui::link::request_body(select)
    else {
        unreachable!()
    };
    assert_eq!(model, "a-private-alpha");
    assert_eq!(
        expected_catalog_key.as_deref(),
        Some("synthetic-producer-51")
    );
}

#[test]
fn configured_id_survives_remote_metadata_retirement_but_owned_session_defaults_do_not() {
    for (attached, owned_defaults) in [(false, false), (true, false), (true, true)] {
        let mut model = live_model();
        let mut driver = LiveDriver::new("test");
        let mut account = oauth_descriptor("custom", "work", true);
        account.incarnation = 61;
        pass(
            &mut driver,
            &mut model,
            LiveReply::Accounts {
                descriptors: vec![account.clone()],
                revision: Some(1),
                sources: vec![],
            },
        );
        let mut profile = private_summary(61, "my-deployment");
        profile.configured_models = vec!["my-deployment".into()];
        profile.model_details[0].supports_vision = Some(true);
        profile.model_details[0].default_effort = Some("high".into());
        pass(
            &mut driver,
            &mut model,
            LiveReply::Providers {
                providers: vec![profile],
                revision: 1,
            },
        );
        if attached {
            let session = haider_protocol::ids::SessionId::new("configured-default-session");
            model.upsert_live_session(&session);
            model.open_session(&session);
        }
        if attached {
            model.apply_scoped_model_selected(
                "custom",
                "my-deployment",
                owned_defaults.then(|| "synthetic-producer-61".into()),
            );
        } else {
            model.apply_model_selected("custom", "my-deployment");
        }
        account.incarnation = 62;
        pass(
            &mut driver,
            &mut model,
            LiveReply::Accounts {
                descriptors: vec![account],
                revision: Some(2),
                sources: vec![],
            },
        );
        let mut next_profile = private_summary(62, "my-deployment");
        next_profile.configured_models = vec!["my-deployment".into()];
        next_profile.model_details[0].source = Some(haider_rpc::ModelDetailSourceWire::Configured);
        next_profile.model_details[0].context_window = None;
        pass(
            &mut driver,
            &mut model,
            LiveReply::Providers {
                providers: vec![next_profile],
                revision: 2,
            },
        );
        let profile = model
            .providers
            .providers
            .iter()
            .find(|profile| profile.provider == "custom")
            .expect("profile");
        assert_eq!(profile.models, vec!["my-deployment"]);
        let detail = &profile.model_details[0];
        assert_eq!(
            detail.source,
            Some(haider_rpc::ModelDetailSourceWire::Configured)
        );
        assert_eq!(detail.context_window, None);
        assert_eq!(detail.supports_vision, None);
        assert_eq!(detail.default_effort, None);
        assert_eq!(model.identity.context_window, 0);
        if owned_defaults {
            assert!(
                model.identity.model_short.is_empty(),
                "captured A defaults require a fresh selection"
            );
            assert!(model.identity_pinned);
        } else {
            assert_eq!(
                model.identity.model_short, "my-deployment",
                "user declaration remains configuration"
            );
        }
    }
}

#[test]
fn restarted_last_model_retains_its_producer_and_retirement_clears_the_file() {
    use haider_tui::settings::SettingsStore;
    let root = tempfile::tempdir().expect("settings");
    let path = root.path().join("tui-settings.json");
    let mut settings = Some(SettingsStore::at(path.clone()));
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    pass(
        &mut driver,
        &mut model,
        LiveReply::Providers {
            providers: vec![private_summary(71, "a-private-alpha")],
            revision: 1,
        },
    );
    model.apply_model_selected("custom", "a-private-alpha");
    let mut seen = 0;
    haider_tui::runtime::sync_model_persistence(&model, &mut seen, &mut settings);
    let mut store = SettingsStore::at(path.clone());
    let pick = store.load_last_model().expect("saved pick");
    assert_eq!(pick.catalog_key.as_deref(), Some("synthetic-producer-71"));
    assert!(pick.scope_known);
    store.set_last_model(Some(pick.clone()));
    store.save_if_changed(haider_tui::theme::ThemeChoice::System);
    assert_eq!(
        SettingsStore::at(path.clone()).load_last_model(),
        Some(pick.clone()),
        "theme write preserves provenance"
    );
    let mut restarted = live_model();
    restarted.restore_last_model_pick(&pick);
    let mut driver = LiveDriver::new("restart");
    restarted.requests.push(AppRequest::CreateSession {
        text: "before any inventory reply".into(),
    });
    let initial = live_pass(&mut driver, &mut restarted, None, std::time::Instant::now());
    let create = initial
        .commands
        .into_iter()
        .find(|command| matches!(command, LiveCommand::Create { .. }))
        .expect("create");
    let haider_rpc::RequestBody::SessionCreateWithPermissionOverrides {
        expected_catalog_key,
        model,
        ..
    } = haider_tui::link::request_body(create)
    else {
        unreachable!()
    };
    assert_eq!(model, "a-private-alpha");
    assert_eq!(
        expected_catalog_key, pick.catalog_key,
        "an early request cannot lose A's owner"
    );
    // An old provider reply may arrive before B's account reply after restart.
    pass(
        &mut driver,
        &mut restarted,
        LiveReply::Providers {
            providers: vec![private_summary(71, "a-private-alpha")],
            revision: 1,
        },
    );
    let mut account = oauth_descriptor("custom", "work", true);
    account.incarnation = 72;
    pass(
        &mut driver,
        &mut restarted,
        LiveReply::Accounts {
            descriptors: vec![account],
            revision: Some(2),
            sources: vec![],
        },
    );
    assert!(
        restarted.identity.model_short.is_empty(),
        "retired A preference cannot become B's current marker"
    );
    let mut seen = 0;
    haider_tui::runtime::sync_model_persistence(&restarted, &mut seen, &mut settings);
    assert_eq!(
        SettingsStore::at(path).load_last_model(),
        None,
        "retirement removes the persisted pick too"
    );
}

#[test]
fn legacy_last_model_waits_for_truth_but_explicit_unowned_configuration_is_preserved() {
    use haider_tui::settings::LastModelPick;
    for scope_known in [false, true] {
        let mut model = live_model();
        let pick = if scope_known {
            LastModelPick {
                provider: "custom".into(),
                model: "manual-unlisted".into(),
                catalog_key: None,
                scope_known: true,
            }
        } else {
            let root = tempfile::tempdir().expect("legacy settings");
            let path = root.path().join("tui-settings.json");
            std::fs::write(&path, r#"{"version":1,"theme":"system","last_provider":"custom","last_model":"manual-unlisted"}"#).expect("legacy file");
            haider_tui::settings::SettingsStore::at(path)
                .load_last_model()
                .expect("legacy pick")
        };
        assert_eq!(pick.scope_known, scope_known);
        model.restore_last_model_pick(&pick);
        let mut driver = LiveDriver::new("test");
        model.requests.push(AppRequest::CreateSession {
            text: "early".into(),
        });
        let initial = live_pass(&mut driver, &mut model, None, std::time::Instant::now());
        let create = initial
            .commands
            .into_iter()
            .find(|command| matches!(command, LiveCommand::Create { .. }))
            .expect("create");
        let haider_rpc::RequestBody::SessionCreateWithPermissionOverrides {
            model: sent,
            resolve_model,
            expected_catalog_key,
            ..
        } = haider_tui::link::request_body(create)
        else {
            unreachable!()
        };
        assert_eq!(sent, if scope_known { "manual-unlisted" } else { "" });
        assert_eq!(resolve_model, !scope_known);
        assert_eq!(expected_catalog_key, None);
        pass(
            &mut driver,
            &mut model,
            LiveReply::Providers {
                providers: vec![private_summary(81, "b-private-beta")],
                revision: 1,
            },
        );
        let mut account = oauth_descriptor("custom", "work", true);
        account.incarnation = 81;
        pass(
            &mut driver,
            &mut model,
            LiveReply::Accounts {
                descriptors: vec![account],
                revision: Some(1),
                sources: vec![],
            },
        );
        assert_eq!(
            model.identity.model_short,
            if scope_known {
                "manual-unlisted"
            } else {
                "b-private-beta"
            }
        );
    }
}

#[test]
fn provider_rebound_retires_owned_selection_without_erasing_explicit_configuration() {
    for (route, owned, retired) in [
        ("same", true, false),
        ("endpoint", true, true),
        ("account", true, true),
        ("provider", true, true),
        ("endpoint", false, false),
        ("account", false, false),
    ] {
        let mut model = live_model();
        let mut driver = LiveDriver::new("test");
        let mut account = oauth_descriptor("custom", "work", true);
        account.incarnation = 71;
        pass(
            &mut driver,
            &mut model,
            LiveReply::Accounts {
                descriptors: vec![account],
                revision: Some(1),
                sources: vec![],
            },
        );
        let mut profile = private_summary(71, "my-deployment");
        profile.configured_models = vec!["my-deployment".into()];
        profile.endpoint = Some("http://127.0.0.1:23456/v1".into());
        pass(
            &mut driver,
            &mut model,
            LiveReply::Providers {
                providers: vec![profile],
                revision: 1,
            },
        );
        let session = haider_protocol::ids::SessionId::new("route-owner-session");
        model.upsert_live_session(&session);
        model.open_session(&session);
        model.apply_scoped_model_selected(
            "custom",
            "my-deployment",
            owned.then(|| "synthetic-producer-71".into()),
        );
        assert_eq!(model.identity.model_short, "my-deployment");
        model.apply_session_provider_rebound(&haider_protocol::session::SessionProviderRebound {
            rebind_id: format!("synthetic-{route}"),
            provider: if route == "provider" {
                "other-custom"
            } else {
                "custom"
            }
            .into(),
            base_url: Some(
                if route == "endpoint" {
                    "http://127.0.0.1:23457/v1"
                } else {
                    "http://127.0.0.1:23456/v1"
                }
                .into(),
            ),
            account: Some(if route == "account" { "other" } else { "work" }.into()),
        });
        if route == "endpoint" || route == "account" {
            assert_eq!(
                model.identity.context_window, 0,
                "{route}: explicit names cannot borrow A's context"
            );
            assert!(model.current_pair_detail().is_none());
            let row = model
                .model_picker_rows()
                .into_iter()
                .find(|row| row.model == "my-deployment")
                .expect("configured ID");
            assert_eq!(row.context_window, None);
            assert_eq!(
                row.source,
                Some(haider_rpc::ModelDetailSourceWire::Configured)
            );
        }
        if retired {
            assert!(
                model.identity.model_short.is_empty(),
                "{route}: derived marker retired"
            );
            assert_eq!(model.identity.context_window, 0);
            assert!(!model.model_picker_rows().iter().any(|row| row.is_current));
            let other = haider_protocol::ids::SessionId::new("route-owner-other-session");
            model.upsert_live_session(&other);
            model.open_session(&other);
            model.open_session(&session);
            assert!(
                model.identity.model_short.is_empty(),
                "{route}: reopening cannot relabel the old choice"
            );
        } else {
            assert_eq!(
                model.identity.model_short, "my-deployment",
                "{route}: negative control"
            );
        }
    }
}

#[test]
fn idle_management_refresh_is_bounded_and_retires_header_without_input() {
    let mut model = live_model();
    let mut driver = LiveDriver::new("idle-refresh");
    let mut account = oauth_descriptor("custom", "work", true);
    account.incarnation = 1;
    pass(
        &mut driver,
        &mut model,
        LiveReply::Accounts {
            descriptors: vec![account.clone()],
            revision: Some(1),
            sources: vec![],
        },
    );
    pass(
        &mut driver,
        &mut model,
        LiveReply::Providers {
            providers: vec![private_summary(1, "a-private")],
            revision: 1,
        },
    );
    assert_eq!(model.identity.model_short, "a-private");
    assert_eq!(
        driver.refresh_management(),
        vec![LiveCommand::AccountList, LiveCommand::ProviderList]
    );
    assert!(driver.refresh_management().is_empty());
    account.incarnation = 2;
    pass(
        &mut driver,
        &mut model,
        LiveReply::Accounts {
            descriptors: vec![account],
            revision: Some(2),
            sources: vec![],
        },
    );
    assert!(model.identity.model_short.is_empty());
    assert_eq!(model.identity.context_window, 0);
    assert!(driver.refresh_management().is_empty());
    pass(
        &mut driver,
        &mut model,
        LiveReply::Providers {
            providers: vec![],
            revision: 2,
        },
    );
    assert_eq!(driver.refresh_management().len(), 2);
}

#[test]
fn picker_and_daemon_share_configured_availability_for_every_provider_kind() {
    for catalog in [
        haider_rpc::ProviderCatalogKindWire::Custom,
        haider_rpc::ProviderCatalogKindWire::Public,
        haider_rpc::ProviderCatalogKindWire::Authenticated,
        haider_rpc::ProviderCatalogKindWire::Offline,
        haider_rpc::ProviderCatalogKindWire::Adapter,
    ] {
        for inventory in [
            haider_rpc::ModelInventoryWire::Configured,
            haider_rpc::ModelInventoryWire::Stale {
                fetched_at_ms: 1,
                reason: "server returned 503 for /models".into(),
            },
            haider_rpc::ModelInventoryWire::Fetched {
                fetched_at_ms: u64::MAX,
            },
        ] {
            for availability in [
                haider_rpc::ProviderAvailabilityWire::Available,
                haider_rpc::ProviderAvailabilityWire::Unavailable,
            ] {
                let mut model = live_model();
                let mut summary = provider_summary("custom", &["deployment"], "deployment");
                summary.catalog = catalog;
                summary.configured_models = vec!["deployment".into()];
                summary.inventory = inventory.clone();
                summary.availability = availability;
                let expected = summary.model_is_selectable("deployment");
                model.providers.apply_snapshot(vec![summary], 1);
                let row = model
                    .model_picker_rows()
                    .into_iter()
                    .find(|row| row.model == "deployment")
                    .expect("configured row");
                assert_eq!(
                    row.available, expected,
                    "{catalog:?} {inventory:?} {availability:?}"
                );
            }
        }
    }
}

#[test]
fn tui_ages_configured_absence_with_shared_policy_and_matching_account_proof() {
    fn publish(
        driver: &mut LiveDriver,
        model: &mut AppModel,
        kind: u8,
        summary: haider_rpc::ProviderSummaryWire,
        revision: u64,
    ) {
        match kind {
            0 => pass(
                driver,
                model,
                LiveReply::Providers {
                    providers: vec![summary],
                    revision,
                },
            ),
            1 => pass(
                driver,
                model,
                LiveReply::ProviderModelsRefreshed {
                    provider: summary,
                    revision,
                },
            ),
            2 => pass(
                driver,
                model,
                LiveReply::DefaultModelSet {
                    command_id: haider_rpc::CommandId::new("catalog-default-reply"),
                    provider: summary,
                    revision,
                },
            ),
            _ => pass(
                driver,
                model,
                LiveReply::ProviderTrustSet {
                    command_id: haider_rpc::CommandId::new("catalog-trust-reply"),
                    provider: summary,
                    revision,
                },
            ),
        }
    }

    fn account(
        driver: &mut LiveDriver,
        model: &mut AppModel,
        alias: &str,
        incarnation: u64,
        status: CredentialStatus,
        revision: u64,
    ) {
        let mut descriptor = oauth_descriptor("custom", alias, true);
        descriptor.auth_method = AuthMethod::ApiKey;
        descriptor.incarnation = incarnation;
        descriptor.status = status;
        pass(
            driver,
            model,
            LiveReply::Accounts {
                descriptors: vec![descriptor],
                revision: Some(revision),
                sources: vec![],
            },
        );
    }

    // Exercise receipt reducers themselves: an older success can arrive
    // after the active account changed, before its provider list arrives.
    for kind in 0..4 {
        let mut model = live_model();
        let mut driver = LiveDriver::new("late-provider-receipt");
        account(&mut driver, &mut model, "work", 1, CredentialStatus::Ok, 1);
        publish(
            &mut driver,
            &mut model,
            0,
            private_summary(1, "private-A"),
            1,
        );
        account(
            &mut driver,
            &mut model,
            "second",
            2,
            CredentialStatus::Ok,
            3,
        );
        publish(
            &mut driver,
            &mut model,
            kind,
            private_summary(1, "private-A"),
            2,
        );
        assert!(
            model.providers.providers[0].models.is_empty(),
            "late receipt={kind}"
        );
        assert!(model.identity.model_short.is_empty(), "late receipt={kind}");
        assert_eq!(model.identity.context_window, 0, "late receipt={kind}");
        assert!(
            !model
                .providers
                .message
                .as_deref()
                .unwrap_or_default()
                .contains("private-A"),
            "late receipt message={kind}"
        );
    }

    let mut model = live_model();
    let mut driver = LiveDriver::new("late-configure-receipt");
    account(&mut driver, &mut model, "work", 1, CredentialStatus::Ok, 1);
    publish(
        &mut driver,
        &mut model,
        0,
        private_summary(1, "private-A"),
        1,
    );
    let configure = driver.handle_request(
        &mut model,
        AppRequest::ProviderConfigure {
            attempt: 1,
            name: "custom".into(),
            origin: "http://127.0.0.1:9/v1".into(),
            model: "deployment".into(),
            keyless: false,
            secret: None,
            family: haider_rpc::ProviderApiFamilyWire::OpenAiResponses,
            models: vec![],
            default_model: None,
            expected_revision: 1,
        },
    );
    let command_id = configure
        .into_iter()
        .find_map(|command| match command {
            LiveCommand::ConfigureProvider { command_id, .. } => Some(command_id),
            _ => None,
        })
        .expect("correlated configure command");
    account(
        &mut driver,
        &mut model,
        "second",
        2,
        CredentialStatus::Ok,
        3,
    );
    let mut summary = private_summary(1, "private-A");
    summary.configured_models = vec!["deployment".into()];
    summary.default_model = Some("deployment".into());
    summary.endpoint = Some("http://127.0.0.1:9/v1".into());
    pass(
        &mut driver,
        &mut model,
        LiveReply::ProviderConfigured {
            command_id,
            provider: summary,
            revision: 2,
        },
    );
    assert!(
        !model.providers.providers[0]
            .models
            .iter()
            .any(|name| name == "private-A")
    );
    assert!(
        model.providers.providers[0]
            .model_details
            .iter()
            .all(|detail| detail.context_window.is_none() && detail.max_output_tokens.is_none())
    );
    assert!(model.identity.model_short.is_empty());
    assert_eq!(model.identity.context_window, 0);

    for remove_provider in [false, true] {
        let mut model = live_model();
        let mut driver = LiveDriver::new("removed-current-identity");
        account(&mut driver, &mut model, "work", 1, CredentialStatus::Ok, 1);
        publish(
            &mut driver,
            &mut model,
            0,
            private_summary(1, "private-A"),
            1,
        );
        assert_eq!(model.identity.model_short, "private-A");
        let command_id = haider_rpc::CommandId::new("catalog-removal-reply");
        let reply = if remove_provider {
            LiveReply::ProviderRemoved {
                command_id,
                provider: "custom".into(),
                revision: 2,
            }
        } else {
            LiveReply::AccountRemoved {
                command_id,
                removed_alias: "work".into(),
                replacement_active_alias: None,
                revision: 2,
            }
        };
        pass(&mut driver, &mut model, reply);
        assert!(
            model.identity.model_short.is_empty(),
            "removed provider={remove_provider}"
        );
        assert_eq!(
            model.identity.context_window, 0,
            "removed provider={remove_provider}"
        );
    }

    for kind in 0..4 {
        for catalog in [
            haider_rpc::ProviderCatalogKindWire::Custom,
            haider_rpc::ProviderCatalogKindWire::Public,
            haider_rpc::ProviderCatalogKindWire::Authenticated,
            haider_rpc::ProviderCatalogKindWire::Offline,
            haider_rpc::ProviderCatalogKindWire::Adapter,
        ] {
            for aged in [false, true] {
                for anonymous in [false, true] {
                    for status in [
                        CredentialStatus::Ok,
                        CredentialStatus::Limited { until_ms: u64::MAX },
                        CredentialStatus::Expired,
                        CredentialStatus::Revoked,
                        CredentialStatus::NeedsAttention {
                            reason: haider_protocol::credential::CredentialAttentionReason::SourceUnreadable,
                        },
                    ] {
                        let mut model = live_model();
                        let mut driver = LiveDriver::new("credential-proof-order");
                        account(&mut driver, &mut model, "work", 1, CredentialStatus::Ok, 1);
                        let mut summary = private_summary(1, "deployment");
                        summary.catalog = catalog;
                        summary.configured_models = vec!["deployment".into()];
                        summary.endpoint = Some("http://127.0.0.1:9/v1".into());
                        summary.api_family = haider_rpc::ProviderApiFamilyWire::OpenAiChatCompletions;
                        summary.auth_methods = if anonymous { vec![] } else { vec![AuthMethod::ApiKey] };
                        summary.inventory = haider_rpc::ModelInventoryWire::Fetched {
                            fetched_at_ms: if aged { 1 } else { u64::MAX },
                        };
                        publish(&mut driver, &mut model, 0, summary.clone(), 1);
                        assert!(model.providers.providers[0].model_is_selectable("deployment"));
                        let expected = anonymous || matches!(status, CredentialStatus::Ok | CredentialStatus::Limited { .. });
                        account(&mut driver, &mut model, "work", 1, status.clone(), 2);
                        let context = format!("publication={kind} catalog={catalog:?} aged={aged} anonymous={anonymous} status={status:?}");
                        assert_eq!(model.providers.providers[0].model_is_selectable("deployment"), expected, "{context}");
                        assert_eq!(model.model_picker_rows().into_iter().find(|row| row.model=="deployment").expect("configured row").available, expected, "{context}");
                        account(&mut driver, &mut model, "work", 1, CredentialStatus::Ok, 3);
                        assert!(model.providers.providers[0].model_is_selectable("deployment"), "recovery {context}");

                        account(&mut driver, &mut model, "work", 1, CredentialStatus::Expired, 4);
                        summary.inventory = haider_rpc::ModelInventoryWire::Unavailable { reason: "synthetic catalog authentication failure".into() };
                        summary.availability = haider_rpc::ProviderAvailabilityWire::Unavailable;
                        publish(&mut driver, &mut model, kind, summary, 5);
                        account(&mut driver, &mut model, "work", 1, CredentialStatus::Ok, 6);
                        assert!(!model.providers.providers[0].model_is_selectable("deployment"), "new negative publication {context}");
                    }
                }
            }
        }

        let mut model = live_model();
        let mut driver = LiveDriver::new("newer-provider-credential-proof");
        account(&mut driver, &mut model, "work", 1, CredentialStatus::Ok, 1);
        let summary = private_summary(1, "deployment");
        publish(&mut driver, &mut model, 0, summary.clone(), 1);
        account(
            &mut driver,
            &mut model,
            "work",
            1,
            CredentialStatus::Expired,
            2,
        );
        assert!(!model.providers.providers[0].model_is_selectable("deployment"));
        publish(&mut driver, &mut model, kind, summary, 3);
        assert!(
            model.providers.providers[0].model_is_selectable("deployment"),
            "new provider proof publication={kind}"
        );
        account(
            &mut driver,
            &mut model,
            "work",
            1,
            CredentialStatus::Expired,
            3,
        );
        assert!(!model.providers.providers[0].model_is_selectable("deployment"));
        account(&mut driver, &mut model, "work", 1, CredentialStatus::Ok, 4);
        assert!(model.providers.providers[0].model_is_selectable("deployment"));
        if kind != 0 {
            publish(
                &mut driver,
                &mut model,
                kind,
                provider_summary("other", &["other-model"], "other-model"),
                6,
            );
            account(
                &mut driver,
                &mut model,
                "work",
                1,
                CredentialStatus::Expired,
                5,
            );
            assert!(
                !model.providers.providers[0].model_is_selectable("deployment"),
                "other provider must not hide credential proof publication={kind}"
            );
        }
    }

    let mut model = live_model();
    let mut driver = LiveDriver::new("retired-credential-proof");
    account(&mut driver, &mut model, "work", 1, CredentialStatus::Ok, 1);
    let mut summary = private_summary(1, "deployment");
    summary.configured_models = vec!["deployment".into()];
    publish(&mut driver, &mut model, 0, summary, 1);
    account(
        &mut driver,
        &mut model,
        "work",
        1,
        CredentialStatus::Expired,
        2,
    );
    account(
        &mut driver,
        &mut model,
        "second",
        2,
        CredentialStatus::Ok,
        3,
    );
    assert!(
        !model.providers.providers[0].model_is_selectable("deployment"),
        "a new account cannot restore the retired credential baseline"
    );

    // Accepted full, refresh, default, and trust publications supersede buffered
    // evidence even when their revision is equal. Rejected older replies do
    // not erase a valid pending projection.
    for kind in 0..4 {
        for publication_revision in [1, 2, 3] {
            let mut model = live_model();
            let mut driver = LiveDriver::new("superseded-listing-proof");
            let mut previous = oauth_descriptor("custom", "previous", true);
            previous.incarnation = 1;
            pass(
                &mut driver,
                &mut model,
                LiveReply::Accounts {
                    descriptors: vec![previous],
                    revision: Some(1),
                    sources: vec![],
                },
            );
            let mut summary = private_summary(2, "deployment");
            summary.configured_models = vec!["deployment".into()];
            summary.endpoint = Some("http://127.0.0.1:9/v1".into());
            summary.api_family = haider_rpc::ProviderApiFamilyWire::OpenAiChatCompletions;
            summary.availability = haider_rpc::ProviderAvailabilityWire::Unavailable;
            publish(&mut driver, &mut model, 0, summary.clone(), 2);
            // The new projection can look exactly like the retired buffer.
            // Equality of visible rows cannot preserve old listing evidence.
            summary.retire_fetched_catalog();
            publish(&mut driver, &mut model, kind, summary, publication_revision);
            let mut current = oauth_descriptor("custom", "work", true);
            current.incarnation = 2;
            pass(
                &mut driver,
                &mut model,
                LiveReply::Accounts {
                    descriptors: vec![current],
                    revision: Some(3),
                    sources: vec![],
                },
            );
            assert_eq!(
                model.providers.providers[0].model_is_selectable("deployment"),
                publication_revision == 1,
                "replacement={kind} publication={publication_revision}"
            );
        }

        // Two rapid switches can publish A's newer catalog before B's
        // delayed account reply. That reply cannot revive B's old buffer or
        // overwrite the new A projection waiting for its own account proof.
        let mut model = live_model();
        let mut driver = LiveDriver::new("superseded-account-proof");
        let mut account_a = oauth_descriptor("custom", "work", true);
        account_a.incarnation = 1;
        pass(
            &mut driver,
            &mut model,
            LiveReply::Accounts {
                descriptors: vec![account_a.clone()],
                revision: Some(1),
                sources: vec![],
            },
        );
        let mut summary_b = private_summary(2, "retired-B");
        summary_b
            .catalog_scope
            .as_mut()
            .expect("scoped private summary")
            .account_alias = Some("second".into());
        publish(&mut driver, &mut model, 0, summary_b, 2);
        publish(
            &mut driver,
            &mut model,
            kind,
            private_summary(1, "latest-A"),
            3,
        );
        let mut account_b = oauth_descriptor("custom", "second", true);
        account_b.incarnation = 2;
        pass(
            &mut driver,
            &mut model,
            LiveReply::Accounts {
                descriptors: vec![account_b],
                revision: Some(2),
                sources: vec![],
            },
        );
        assert!(
            model.providers.providers[0].models.is_empty(),
            "replacement={kind}"
        );
        pass(
            &mut driver,
            &mut model,
            LiveReply::Accounts {
                descriptors: vec![account_a],
                revision: Some(3),
                sources: vec![],
            },
        );
        assert_eq!(
            model.providers.providers[0].models,
            ["latest-A"],
            "replacement={kind}"
        );
    }

    // A partial publication for a different provider preserves this one's
    // pending proof, even though it advances the global management revision.
    for kind in [1, 2, 3] {
        let mut model = live_model();
        let mut driver = LiveDriver::new("independent-provider-proof");
        let mut previous = oauth_descriptor("custom", "previous", true);
        previous.incarnation = 1;
        pass(
            &mut driver,
            &mut model,
            LiveReply::Accounts {
                descriptors: vec![previous],
                revision: Some(1),
                sources: vec![],
            },
        );
        publish(&mut driver, &mut model, 0, private_summary(2, "pending"), 2);
        publish(
            &mut driver,
            &mut model,
            kind,
            provider_summary("other", &["other-model"], "other-model"),
            3,
        );
        let mut current = oauth_descriptor("custom", "work", true);
        current.incarnation = 2;
        pass(
            &mut driver,
            &mut model,
            LiveReply::Accounts {
                descriptors: vec![current],
                revision: Some(3),
                sources: vec![],
            },
        );
        assert_eq!(model.providers.providers[0].models, ["pending"]);
    }

    // A same/older publication cannot assert a different active owner.
    // A later account reply must not revive that discarded listing proof.
    for previous_incarnation in [1, 42] {
        for publication_revision in [1, 2] {
            let mut model = live_model();
            let mut driver = LiveDriver::new("nonfuture-catalog-proof");
            let mut previous = oauth_descriptor("custom", "previous", true);
            previous.auth_method = AuthMethod::ApiKey;
            previous.incarnation = previous_incarnation;
            pass(
                &mut driver,
                &mut model,
                LiveReply::Accounts {
                    descriptors: vec![previous],
                    revision: Some(2),
                    sources: vec![],
                },
            );
            let mut summary = private_summary(1, "deployment");
            summary.auth_methods = vec![AuthMethod::ApiKey];
            summary.models.clear();
            summary.model_details.clear();
            summary.configured_models = vec!["deployment".into()];
            summary.endpoint = Some("http://127.0.0.1:9/v1".into());
            summary.api_family = haider_rpc::ProviderApiFamilyWire::OpenAiChatCompletions;
            summary.inventory = haider_rpc::ModelInventoryWire::Fetched { fetched_at_ms: 1 };
            summary.availability = haider_rpc::ProviderAvailabilityWire::Unavailable;
            pass(
                &mut driver,
                &mut model,
                LiveReply::Providers {
                    providers: vec![summary],
                    revision: publication_revision,
                },
            );
            let mut current = oauth_descriptor("custom", "work", true);
            current.auth_method = AuthMethod::ApiKey;
            current.incarnation = 1;
            pass(
                &mut driver,
                &mut model,
                LiveReply::Accounts {
                    descriptors: vec![current],
                    revision: Some(3),
                    sources: vec![],
                },
            );
            assert!(
                !model.providers.providers[0].model_is_selectable("deployment"),
                "discarded publication={publication_revision} previous={previous_incarnation}"
            );
        }
    }
    for previous_incarnation in [None, Some(1), Some(42)] {
        for catalog in [
            haider_rpc::ProviderCatalogKindWire::Custom,
            haider_rpc::ProviderCatalogKindWire::Public,
            haider_rpc::ProviderCatalogKindWire::Authenticated,
            haider_rpc::ProviderCatalogKindWire::Offline,
            haider_rpc::ProviderCatalogKindWire::Adapter,
        ] {
            for aged in [false, true] {
                for other_listed in [false, true] {
                    for accounts_first in [false, true] {
                        for (state, status, active) in [
                            ("anonymous", None, false),
                            ("ok", Some(CredentialStatus::Ok), true),
                            (
                                "limited",
                                Some(CredentialStatus::Limited { until_ms: u64::MAX }),
                                true,
                            ),
                            ("expired", Some(CredentialStatus::Expired), true),
                            ("revoked", Some(CredentialStatus::Revoked), true),
                            (
                                "attention",
                                Some(CredentialStatus::NeedsAttention {
                                    reason: haider_protocol::credential::CredentialAttentionReason::SourceUnreadable,
                                }),
                                true,
                            ),
                            ("inactive", Some(CredentialStatus::Ok), false),
                            ("missing", None, true),
                            ("foreign", Some(CredentialStatus::Ok), true),
                        ] {
                            let mut model = live_model();
                            let mut driver = LiveDriver::new("tui-freshness-boundary");
                            if let Some(incarnation) = previous_incarnation {
                                let mut previous = oauth_descriptor("custom", "previous", true);
                                previous.auth_method = AuthMethod::ApiKey;
                                previous.incarnation = incarnation;
                                pass(&mut driver, &mut model, LiveReply::Accounts {
                                    descriptors: vec![previous],
                                    revision: Some(1),
                                    sources: vec![],
                                });
                            }
                            let listed = if other_listed {
                                &["another-listed"][..]
                            } else {
                                &[][..]
                            };
                            let mut summary = provider_summary("custom", listed, "");
                            summary.catalog = catalog;
                            summary.endpoint = Some("http://127.0.0.1:9/v1".into());
                            summary.api_family = haider_rpc::ProviderApiFamilyWire::OpenAiChatCompletions;
                            summary.configured_models = vec!["deployment".into()];
                            summary.default_model = None;
                            summary.inventory = haider_rpc::ModelInventoryWire::Fetched {
                                fetched_at_ms: if aged { 1 } else { u64::MAX },
                            };
                            summary.inventory_authority =
                                haider_rpc::ModelInventoryAuthorityWire::Authoritative;
                            summary.availability = haider_rpc::ProviderAvailabilityWire::Unavailable;
                            summary.availability_reason =
                                Some("deployment is absent from the latest model list".into());
                            if state == "anonymous" {
                                summary.auth_methods.clear();
                            } else {
                                summary.auth_methods = vec![AuthMethod::ApiKey];
                                summary.catalog_scope = Some(haider_rpc::ProviderCatalogScopeWire {
                                    key: "synthetic-producer-1".into(),
                                    account_alias: Some("work".into()),
                                    account_incarnation: Some(1),
                                });
                            }
                            let descriptors = status
                                .map(|status| {
                                    let provider = if state == "foreign" {
                                        "elsewhere"
                                    } else {
                                        "custom"
                                    };
                                    let mut descriptor = oauth_descriptor(provider, "work", active);
                                    descriptor.auth_method = AuthMethod::ApiKey;
                                    descriptor.incarnation = 1;
                                    descriptor.status = status;
                                    vec![descriptor]
                                })
                                .unwrap_or_default();
                            let accounts = LiveReply::Accounts {
                                descriptors,
                                revision: Some(2),
                                sources: vec![],
                            };
                            if accounts_first {
                                pass(&mut driver, &mut model, accounts.clone());
                            }
                            pass(
                                &mut driver,
                                &mut model,
                                LiveReply::Providers {
                                    providers: vec![summary],
                                    revision: 2,
                                },
                            );
                            if !accounts_first {
                                // Until the matching account arrives, an authenticated
                                // unavailable row cannot acquire readiness from expiry.
                                if state != "anonymous" {
                                    assert!(!model.providers.providers[0].model_is_selectable("deployment"));
                                }
                                pass(&mut driver, &mut model, accounts);
                            }
                            let expected = aged && matches!(state, "anonymous" | "ok" | "limited");
                            let context = format!(
                                "{catalog:?} aged={aged} other={other_listed} accounts_first={accounts_first} state={state} previous_incarnation={previous_incarnation:?}"
                            );
                            let summary = &model.providers.providers[0];
                            assert_eq!(summary.model_is_selectable("deployment"), expected, "{context}");
                            if expected {
                                assert!(matches!(summary.inventory, haider_rpc::ModelInventoryWire::Stale { .. }), "{context}");
                                assert!(summary.selection_reason(Some("deployment")).unwrap_or_default().contains("refresh"), "{context}");
                                assert!(summary.model_details.iter().filter(|row| row.name == "deployment").all(|row| row.context_window.is_none() && row.max_output_tokens.is_none()), "{context}");
                                let row = model.model_picker_rows().into_iter().find(|row| row.model == "deployment").expect("aged configuration is visible");
                                assert!(row.available, "{context}");
                            } else if let Some(row) = model.model_picker_rows().into_iter().find(|row| row.model == "deployment") {
                                assert!(!row.available, "{context}");
                            }
                            if aged && matches!(state, "expired" | "revoked" | "attention") {
                                // Readiness can recover before another provider reply.
                                // Same-incarnation listing proof must survive the
                                // unhealthy account reply in either arrival order.
                                let mut descriptor = oauth_descriptor("custom", "work", true);
                                descriptor.auth_method = AuthMethod::ApiKey;
                                descriptor.incarnation = 1;
                                pass(&mut driver, &mut model, LiveReply::Accounts {
                                    descriptors: vec![descriptor],
                                    revision: Some(3),
                                    sources: vec![],
                                });
                                assert!(model.providers.providers[0].model_is_selectable("deployment"), "recovered: {context}");
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn restored_private_preference_is_hidden_until_live_owner_is_proven() {
    for scope_known in [false, true] {
        let mut model = live_model();
        let pick = haider_tui::settings::LastModelPick {
            provider: "custom".into(),
            model: "a-private".into(),
            catalog_key: scope_known.then(|| "synthetic-producer-1".into()),
            scope_known,
        };
        model.restore_last_model_pick(&pick);
        model.identity.context_window = 123_456;
        assert!(model.display_identity().model_short.is_empty());
        assert_eq!(model.display_identity().context_window, 0);
        assert_eq!(model.context_meter().window, None);
        assert_eq!(model.composer_identity(118), None);
        let mut driver = LiveDriver::new("restored-owner");
        pass(
            &mut driver,
            &mut model,
            LiveReply::Providers {
                providers: vec![private_summary(2, "b-private")],
                revision: 2,
            },
        );
        assert!(!model.display_identity().model_short.contains("a-private"));
        let mut account = oauth_descriptor("custom", "work", true);
        account.incarnation = 2;
        pass(
            &mut driver,
            &mut model,
            LiveReply::Accounts {
                descriptors: vec![account],
                revision: Some(2),
                sources: vec![],
            },
        );
        assert_eq!(model.display_identity().model_short, "b-private");
    }
}

#[test]
fn retired_private_session_identity_clears_account_and_window_in_either_reply_order() {
    for attached in [false, true] {
        for accounts_first in [false, true] {
            let mut model = live_model();
            let mut driver = LiveDriver::new("retired-session-owner");
            let mut a = oauth_descriptor("custom", "work", true);
            a.incarnation = 1;
            pass(
                &mut driver,
                &mut model,
                LiveReply::Accounts {
                    descriptors: vec![a],
                    revision: Some(1),
                    sources: vec![],
                },
            );
            pass(
                &mut driver,
                &mut model,
                LiveReply::Providers {
                    providers: vec![private_summary(1, "a-private")],
                    revision: 1,
                },
            );
            model.apply_model_selected("custom", "a-private");
            assert_eq!(model.identity.account, "work");
            if attached {
                model.active_session =
                    Some(haider_protocol::ids::SessionId::new("synthetic-session"));
                model.screen = haider_tui::app::Screen::Session;
            }
            let mut b = oauth_descriptor("custom", "replacement", true);
            b.incarnation = 2;
            let mut inventory = private_summary(2, "b-private");
            inventory
                .catalog_scope
                .as_mut()
                .expect("scope")
                .account_alias = Some("replacement".into());
            inventory.models.clear();
            inventory.model_details.clear();
            inventory.default_model = None;
            inventory.inventory = haider_rpc::ModelInventoryWire::Unavailable {
                reason: "synthetic 503".into(),
            };
            inventory.availability = haider_rpc::ProviderAvailabilityWire::Unavailable;
            let accounts = LiveReply::Accounts {
                descriptors: vec![b],
                revision: Some(2),
                sources: vec![],
            };
            let providers = LiveReply::Providers {
                providers: vec![inventory],
                revision: 2,
            };
            let replies = if accounts_first {
                [accounts, providers]
            } else {
                [providers, accounts]
            };
            for reply in replies {
                pass(&mut driver, &mut model, reply);
            }
            assert!(model.display_identity().model_short.is_empty());
            assert_ne!(model.display_identity().account, "work");
            assert_eq!(model.context_meter().window, None);
            assert_eq!(model.composer_identity(118), None);
            for width in [118, 80] {
                assert!(haider_tui::render::status_left_string(&model, width).contains("unknown"));
            }
        }
    }
}
