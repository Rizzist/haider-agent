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

/// MUTATION CHECK: changing the documented fifteen-minute TTL comparison
/// from `>=` to `>` makes the exact-boundary provider disappear.
#[test]
fn automatic_refresh_starts_at_the_exact_ttl_boundary() {
    let just_fresh = haider_rpc::MODEL_INVENTORY_TTL_MS - 1;
    let at_boundary = haider_rpc::MODEL_INVENTORY_TTL_MS;
    let document = document(vec![
        provider("fresh", Some(1), Some(just_fresh)),
        provider("stale", Some(2), Some(at_boundary)),
        provider("seeded", None, None),
    ]);
    assert_eq!(refresh_targets(&document, None), vec!["stale"]);
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
