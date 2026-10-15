#![allow(clippy::expect_used)]

use super::*;

fn provider(name: &str, fetched_at_ms: Option<u64>, inventory_age_ms: Option<u64>) -> ProviderView {
    ProviderView {
        provider: name.to_owned(),
        api_family: ProviderApiFamilyWire::OpenAiChatCompletions,
        endpoint: Some("http://127.0.0.1:11434/v1".to_owned()),
        enabled: true,
        availability: "available",
        availability_reason: None,
        auth_state: "not_required",
        has_credential: false,
        auth_methods: Vec::new(),
        default_model: Some("local-model".to_owned()),
        fetched_at_ms,
        inventory_age_ms,
        catalog: haider_rpc::ProviderCatalogKindWire::Custom,
        inventory: match (fetched_at_ms, inventory_age_ms) {
            (Some(fetched_at_ms), Some(age)) if age >= haider_rpc::MODEL_INVENTORY_TTL_MS => {
                haider_rpc::ModelInventoryWire::Stale {
                    fetched_at_ms,
                    reason: "refresh due".into(),
                }
            }
            (Some(fetched_at_ms), _) => haider_rpc::ModelInventoryWire::Fetched { fetched_at_ms },
            (None, _) => haider_rpc::ModelInventoryWire::NeverFetched,
        },
        models: vec![model_view("local-model".to_owned())],
    }
}

fn document(providers: Vec<ProviderView>) -> ModelsDocument {
    ModelsDocument {
        schema: MODELS_SCHEMA,
        revision: 7,
        providers,
    }
}

#[test]
fn refresh_parser_distinguishes_global_and_provider_scopes() {
    assert_eq!(
        parse_options(&["--refresh".to_owned()]).expect("global refresh"),
        Some(ModelsOptions {
            json: false,
            refresh: Some(ModelsRefresh::All),
        })
    );
    assert_eq!(
        parse_options(&[
            "--json".to_owned(),
            "--refresh".to_owned(),
            "local-router".to_owned(),
        ])
        .expect("provider refresh"),
        Some(ModelsOptions {
            json: true,
            refresh: Some(ModelsRefresh::Provider("local-router".to_owned())),
        })
    );
    assert!(parse_options(&["--refresh".to_owned(), "--refresh".to_owned()]).is_err());
}

/// MUTATION: implicitly refresh a stale row on an ordinary read. A held
/// listing would then hide otherwise selectable cached inventory until timeout.
#[test]
fn default_read_never_waits_for_stale_inventory_refresh() {
    let just_fresh = haider_rpc::MODEL_INVENTORY_TTL_MS - 1;
    let at_boundary = haider_rpc::MODEL_INVENTORY_TTL_MS;
    let document = document(vec![
        provider("fresh", Some(1), Some(just_fresh)),
        provider("stale", Some(2), Some(at_boundary)),
        provider("older", Some(3), Some(at_boundary + 1)),
        provider("seeded", None, None),
    ]);
    assert!(refresh_targets(&document, None).is_empty());
    assert_eq!(document.providers[1].availability, "available");
    assert_eq!(document.providers[1].models[0].model, "local-model");
    assert_eq!(
        refresh_targets(&document, Some(&ModelsRefresh::Provider("stale".into()))),
        vec!["stale"]
    );
}

/// Explicit global refresh includes a configured compatible source even when
/// it has no prior fetch timestamp; static providers without catalogs are
/// excluded by `provider_supports_live_discovery`.
#[test]
fn global_refresh_targets_every_fetched_provider() {
    let document = document(vec![
        provider("first", Some(1), Some(0)),
        provider("second", Some(2), Some(0)),
        provider("new-custom", None, None),
    ]);
    assert_eq!(
        refresh_targets(&document, Some(&ModelsRefresh::All)),
        vec!["first", "second", "new-custom"]
    );
    assert_eq!(
        refresh_targets(
            &document,
            Some(&ModelsRefresh::Provider("bedrock".to_owned()))
        ),
        vec!["bedrock"]
    );
}

/// MUTATION CHECK: omitting either JSON field, or fabricating zero for an
/// unfetched inventory, changes this public `haider.models.v1` projection.
#[test]
fn models_json_publishes_fetch_time_and_age_without_fabrication() {
    let value = serde_json::to_value(document(vec![
        provider("live", Some(1_700_000_000_000), Some(42_000)),
        provider("seeded", None, None),
    ]))
    .expect("models JSON");
    assert_eq!(value["providers"][0]["fetched_at"], 1_700_000_000_000_u64);
    assert_eq!(value["providers"][0]["inventory_age"], 42_000_u64);
    assert!(value["providers"][1]["fetched_at"].is_null());
    assert!(value["providers"][1]["inventory_age"].is_null());
}

#[test]
fn one_oauth_failure_keeps_other_provider_rows_and_its_own_reason() {
    let mut document = document(vec![
        provider("openai-oauth", None, None),
        provider("haider-code", Some(10), Some(0)),
    ]);
    let public_before = serde_json::to_value(&document.providers[1]).expect("public row");
    apply_refresh_failures(
        &mut document,
        BTreeMap::from([("openai-oauth".into(), "OAuth credential expired".into())]),
    )
    .expect("partial listing");
    assert_eq!(document.providers.len(), 2);
    assert!(
        matches!(&document.providers[0].inventory, haider_rpc::ModelInventoryWire::Unavailable { reason } if reason == "OAuth credential expired")
    );
    assert_eq!(
        serde_json::to_value(&document.providers[1]).expect("public row"),
        public_before
    );
}

#[test]
fn legacy_flat_seed_cannot_override_never_fetched_provenance() {
    let summary: ProviderSummaryWire = serde_json::from_value(serde_json::json!({
        "provider": "haider-code", "api_family": "openai_chat_completions",
        "models": ["Go"], "default_model": "Go", "availability": "available", "enabled": true,
    }))
    .expect("legacy summary");
    let view = provider_view(summary, &[], 100);
    assert_eq!(view.inventory, haider_rpc::ModelInventoryWire::NeverFetched);
    assert!(view.models.is_empty());
    assert!(view.default_model.is_none());
    assert_eq!(view.availability, "unavailable");
}

#[test]
fn cli_preserves_static_rows_when_subscription_catalog_returns_403() {
    let summary: ProviderSummaryWire = serde_json::from_value(serde_json::json!({
        "provider": "anthropic-oauth", "api_family": "anthropic_messages",
        "models": ["claude-fable-5-1"],
        "model_details": [{"name":"claude-fable-5-1","context_window":1000000,"source":"static"}],
        "inventory": {"state":"unavailable","reason":"catalog returned 403"},
        "default_model": "claude-fable-5-1", "availability": "available", "enabled": true
    }))
    .expect("summary");
    let view = provider_view(summary, &[], 100);
    assert_eq!(view.availability, "available");
    assert_eq!(view.default_model.as_deref(), Some("claude-fable-5-1"));
    let value = serde_json::to_value(&view).expect("CLI JSON");
    assert_eq!(value["models"][0]["source"], "static");
    assert_eq!(value["models"][0]["context_window"], 1_000_000);
}

#[test]
fn cli_projects_configured_rows_when_a_fresh_empty_list_ages_before_render() {
    let summary: ProviderSummaryWire = serde_json::from_value(serde_json::json!({
        "provider":"work", "api_family":"openai_chat_completions",
        "endpoint":"http://127.0.0.1:19373/v1", "enabled":true,
        "models":[], "configured_models":["deployment"],
        "inventory":{"state":"fetched", "fetched_at_ms":100},
        "inventory_authority":"authoritative", "availability":"unavailable"
    }))
    .expect("fresh empty list");
    let fresh = provider_view(
        summary.clone(),
        &[],
        100 + haider_rpc::MODEL_INVENTORY_TTL_MS - 1,
    );
    assert_eq!(fresh.availability, "unavailable");
    assert!(fresh.models.is_empty());
    let aged = provider_view(summary, &[], 100 + haider_rpc::MODEL_INVENTORY_TTL_MS);
    assert_eq!(aged.availability, "available");
    assert_eq!(aged.models.len(), 1);
    assert_eq!(aged.models[0].model, "deployment");
    assert!(aged.models[0].context_window.is_none());
    assert!(
        aged.availability_reason
            .expect("stale reason")
            .contains("refresh")
    );
}

#[test]
fn provider_availability_refusals_keep_provider_exit_status() {
    for code in [
        "provider_error",
        "provider_timeout",
        "credential_missing",
        "credential_limited",
        "unauthorized",
        "provider_unavailable",
    ] {
        for retryable in [false, true] {
            let error = ModelsError::Rpc {
                code: code.into(),
                message: "fixture refusal".into(),
                retryable,
            };
            assert_eq!(failure(&error), ExitCode::from(EX_PROVIDER), "{code}");
        }
    }
    for code in [
        "internal",
        "busy",
        "PROVIDER_UNAVAILABLE",
        " provider_unavailable",
        "provider_unavailable_extra",
        "provider_unavailable ",
        "",
    ] {
        for retryable in [false, true] {
            let error = ModelsError::Rpc {
                code: code.into(),
                message: "unknown refusal".into(),
                retryable,
            };
            assert_eq!(failure(&error), ExitCode::from(EX_SOFTWARE), "{code}");
        }
    }
}
