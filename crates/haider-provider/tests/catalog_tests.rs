//! W5e-2 discovery: parse BOTH providers' real shapes, honour the
//! provider's own visibility/priority, and never synthesize a list.
//!
//! The OpenAI fixture is the SHAPE captured from the installed codex CLI's
//! live `~/.codex/models_cache.json` on 2026-07-30 (slugs deliberately
//! neutralized — this suite must never assert that a particular model
//! exists, only that whatever the provider names round-trips faithfully).
#![allow(clippy::expect_used)]

use haider_provider::{
    CatalogError, CatalogSource, DiscoveredModel, openai_compatible_catalog_endpoint,
    parse_catalog, pickable,
};

fn codex_payload() -> serde_json::Value {
    serde_json::json!({
        "fetched_at": "2026-07-30T08:02:48.319581Z",
        "etag": "W/\"6a9f8a8701491e33b9bc0a5fbb8d7f18\"",
        "client_version": "0.146.0",
        "models": [
            {
                "slug": "frontier-a",
                "display_name": "Frontier A",
                "description": "Latest frontier agentic coding model.",
                "default_reasoning_level": "low",
                "supported_reasoning_levels": [
                    {"effort": "low", "description": "Fast responses"},
                    {"effort": "medium", "description": "Balanced"},
                    {"effort": "high", "description": "Deeper"},
                    {"effort": "xhigh", "description": "Extra"},
                    {"effort": "max", "description": "Maximum"},
                    {"effort": "ultra", "description": "Delegating"}
                ],
                "visibility": "list",
                "supported_in_api": true,
                "priority": 1,
                "base_instructions": "You are an agent…"
            },
            {
                "slug": "frontier-b",
                "display_name": "Frontier B",
                "supported_reasoning_levels": [{"effort": "medium"}],
                "visibility": "list",
                "priority": 3
            },
            {
                "slug": "internal-only",
                "display_name": "Internal Only",
                "visibility": "hidden",
                "priority": 2
            }
        ]
    })
}

fn anthropic_payload() -> serde_json::Value {
    serde_json::json!({
        "data": [
            {"id": "model-opus", "display_name": "Model Opus", "type": "model"},
            {"id": "model-sonnet", "display_name": "Model Sonnet", "type": "model"}
        ],
        "has_more": false
    })
}

/// The codex shape round-trips faithfully: display names, the effort ladder,
/// the declared default, and the provider's priority.
#[test]
fn codex_shape_parses_with_its_effort_ladder() {
    let models = parse_catalog(CatalogSource::OpenAiSubscription, &codex_payload())
        .expect("codex payload parses");
    assert_eq!(models.len(), 3, "every named model parses, hidden included");
    let first = models
        .iter()
        .find(|model| model.slug == "frontier-a")
        .expect("frontier-a");
    assert_eq!(first.display_name, "Frontier A");
    assert_eq!(first.default_effort.as_deref(), Some("low"));
    assert_eq!(
        first.supported_efforts,
        vec!["low", "medium", "high", "xhigh", "max", "ultra"],
        "the provider's OWN ladder, not a guessed one"
    );
    assert_eq!(first.priority, Some(1));
    assert!(first.visible);
    assert!(first.description.is_some());
}

/// MUTATION CHECK: hardcode `context_window` to `None` in the codex parse
/// arm. Expected runtime failure: the provider's declared token limit is
/// missing below.
#[test]
fn codex_declared_context_window_is_preserved() {
    let models = parse_catalog(
        CatalogSource::OpenAiSubscription,
        &serde_json::json!({
            "models": [
                {"slug": "declared", "context_window": 200_000},
                {"slug": "largest-positive", "context_window": u64::MAX}
            ]
        }),
    )
    .expect("codex payload parses");
    assert_eq!(models[0].context_window, Some(200_000));
    assert_eq!(models[1].context_window, Some(u64::MAX));
}

/// MUTATION CHECK: substitute a numeric default when `context_window` is
/// absent. Expected runtime failure: the missing provider declaration becomes
/// a guessed value instead of `None`.
#[test]
fn absent_codex_context_window_stays_none() {
    let models = parse_catalog(
        CatalogSource::OpenAiSubscription,
        &serde_json::json!({"models": [{"slug": "undeclared"}]}),
    )
    .expect("codex payload parses");
    assert_eq!(models[0].context_window, None);
}

/// MUTATION CHECK: remove the positive-value filter from the codex parse arm.
/// Expected runtime failure: the zero declaration survives instead of being
/// treated as absent.
#[test]
fn zero_codex_context_window_is_absent() {
    let models = parse_catalog(
        CatalogSource::OpenAiSubscription,
        &serde_json::json!({
            "models": [
                {"slug": "zero", "context_window": 0},
                {"slug": "negative", "context_window": -1}
            ]
        }),
    )
    .expect("codex payload parses");
    assert_eq!(models[0].context_window, None);
    assert_eq!(models[1].context_window, None);
}

/// MUTATION CHECK: read `context_window` through a provider-agnostic parse
/// path. Expected runtime failure: the injected Anthropic value appears even
/// though Anthropic's catalog contract never declares context windows.
#[test]
fn anthropic_context_window_is_always_none() {
    let models = parse_catalog(
        CatalogSource::AnthropicSubscription,
        &serde_json::json!({
            "data": [{"id": "model-opus", "context_window": 200_000}]
        }),
    )
    .expect("anthropic payload parses");
    assert_eq!(models[0].context_window, None);
}

/// MUTATION CHECK: make `visible` unconditionally `true` (drop the
/// `visibility == "list"` test). Expected runtime failure: the hidden model
/// appears in `pickable` below.
/// Verified by revert on 2026-07-30.
#[test]
fn provider_visibility_and_priority_drive_the_picker() {
    let models =
        parse_catalog(CatalogSource::OpenAiSubscription, &codex_payload()).expect("parses");
    let listed = pickable(&models);
    let slugs: Vec<&str> = listed.iter().map(|model| model.slug.as_str()).collect();
    assert_eq!(
        slugs,
        vec!["frontier-a", "frontier-b"],
        "hidden models are dropped and priority orders the rest"
    );
    assert!(
        !slugs.contains(&"internal-only"),
        "a provider-hidden model must never reach a picker"
    );
}

/// Anthropic's `/v1/models` shape: `data[].id`, no visibility or effort
/// fields — which must yield an EMPTY ladder, never a fabricated one.
#[test]
fn anthropic_shape_parses_without_inventing_efforts() {
    let models = parse_catalog(CatalogSource::AnthropicSubscription, &anthropic_payload())
        .expect("anthropic payload parses");
    assert_eq!(models.len(), 2);
    for model in &models {
        assert!(
            model.supported_efforts.is_empty(),
            "no effort ladder may be invented for a provider that declares none"
        );
        assert!(model.default_effort.is_none());
        assert!(model.visible, "absent visibility means listable");
    }
    assert_eq!(models[0].slug, "model-opus");
    assert_eq!(models[0].display_name, "Model Opus");
}

/// A response with no model array, or entries with no id, is UNAVAILABLE —
/// the caller falls back to its cache. Discovery never manufactures a list.
#[test]
fn malformed_payloads_are_unavailable_not_substituted() {
    assert!(
        parse_catalog(
            CatalogSource::OpenAiSubscription,
            &serde_json::json!({"unexpected": true})
        )
        .is_err(),
        "a payload with no model array must not yield models"
    );
    // Entries without a usable id are skipped, not guessed.
    let models = parse_catalog(
        CatalogSource::AnthropicSubscription,
        &serde_json::json!({"data": [{"display_name": "No Id"}, {"id": "real"}]}),
    )
    .expect("parses");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].slug, "real");
}

/// MUTATION CHECK (W5g-5b): reuse the vendor parser's metadata fields for an
/// OpenAI-compatible catalog. Expected runtime failure: the injected display
/// name, context window, effort ladder, visibility, or priority survives even
/// though this discovery contract declares only `data[].id`.
#[test]
fn openai_compatible_ids_are_models_without_invented_metadata() {
    let source = CatalogSource::OpenAiCompatible {
        origin: "https://models.example.invalid/v1".to_owned(),
    };
    let models = parse_catalog(
        source,
        &serde_json::json!({
            "object": "list",
            "data": [{
                "id": "custom-model-a",
                "display_name": "Injected Label",
                "context_window": 123_456,
                "description": "Injected description",
                "default_reasoning_level": "high",
                "supported_reasoning_levels": ["high"],
                "visibility": "hidden",
                "priority": 1
            }, {
                "id": "custom-model-b"
            }]
        }),
    )
    .expect("OpenAI-compatible payload parses");
    assert_eq!(
        models,
        vec![
            DiscoveredModel {
                slug: "custom-model-a".to_owned(),
                display_name: "custom-model-a".to_owned(),
                context_window: None,
                description: None,
                default_effort: None,
                supported_efforts: Vec::new(),
                visible: true,
                priority: None,
                extensions: None,
            },
            DiscoveredModel {
                slug: "custom-model-b".to_owned(),
                display_name: "custom-model-b".to_owned(),
                context_window: None,
                description: None,
                default_effort: None,
                supported_efforts: Vec::new(),
                visible: true,
                priority: None,
                extensions: None,
            },
        ]
    );
}

/// MUTATION CHECK (W5g-5b): substitute a built-in model when the custom
/// response has no `data` array. Expected runtime failure: this malformed
/// payload returns models instead of the runtime `Unavailable` error.
#[test]
fn malformed_openai_compatible_catalog_is_unavailable_not_substituted() {
    let error = parse_catalog(
        CatalogSource::OpenAiCompatible {
            origin: "https://models.example.invalid/v1".to_owned(),
        },
        &serde_json::json!({"object": "list", "models": [{"id": "wrong-shape"}]}),
    )
    .expect_err("missing data array must be unavailable");
    assert!(matches!(error, CatalogError::Unavailable { .. }));
}

/// MUTATION CHECK (W5g-5b): remove the loopback-only HTTP check before
/// discovery. Expected runtime failure: the remote HTTP origin below becomes
/// fetchable instead of failing validation before any network request.
///
/// MUTATION CHECK: allow URL userinfo or fragments. Expected runtime failure:
/// either credential-bearing target below passes the discovery-time backstop.
#[test]
fn custom_catalog_origin_policy_rejects_remote_http_before_fetch() {
    let error = openai_compatible_catalog_endpoint("http://203.0.113.7/v1")
        .expect_err("remote HTTP must be refused before fetch");
    assert!(matches!(error, CatalogError::Unavailable { .. }));
    assert!(
        openai_compatible_catalog_endpoint("https://user@example.invalid/v1").is_err(),
        "userinfo must be refused before fetch"
    );
    assert!(
        openai_compatible_catalog_endpoint("https://models.example.invalid/v1#fragment").is_err(),
        "fragments must be refused before fetch"
    );

    assert_eq!(
        openai_compatible_catalog_endpoint("https://models.example.invalid/v1")
            .expect("remote HTTPS is allowed"),
        "https://models.example.invalid/v1/models"
    );
    assert_eq!(
        openai_compatible_catalog_endpoint("http://127.42.0.9:11434/v1/")
            .expect("IPv4 loopback HTTP is allowed"),
        "http://127.42.0.9:11434/v1/models"
    );
    assert_eq!(
        openai_compatible_catalog_endpoint("http://[::1]:11434/v1")
            .expect("IPv6 loopback HTTP is allowed"),
        "http://[::1]:11434/v1/models"
    );
    assert_eq!(
        openai_compatible_catalog_endpoint("http://localhost:11434/v1")
            .expect("localhost HTTP is allowed"),
        "http://localhost:11434/v1/models"
    );
}

/// The endpoints are exactly the vendors' own — asserted so a refactor
/// cannot quietly point discovery somewhere else.
#[test]
fn endpoints_are_the_vendor_paths() {
    assert_eq!(
        CatalogSource::OpenAiSubscription.endpoint(),
        "https://chatgpt.com/backend-api/codex/models",
        "the codex CLI's own catalog endpoint"
    );
    assert_eq!(
        CatalogSource::AnthropicSubscription.endpoint(),
        "https://api.anthropic.com/v1/models"
    );
    assert_eq!(
        CatalogSource::KimiOAuth.endpoint(),
        "https://api.kimi.com/coding/v1/models"
    );
}

/// MUTATION CHECK: parse Kimi through the generic compatible shape, retain
/// Anthropic-protocol rows, or guess capability flags. Expected RUNTIME
/// failure: the provider fixture below changes its exact declared facts.
#[test]
fn models_catalog_parses_context_length_and_skips_anthropic_protocol() {
    let payload: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/catalog/kimi_models.json"))
            .expect("Kimi models fixture");
    let models = parse_catalog(CatalogSource::KimiOAuth, &payload).expect("Kimi catalog parses");
    assert_eq!(
        models
            .iter()
            .map(|model| model.slug.as_str())
            .collect::<Vec<_>>(),
        vec!["kimi-coding-a", "kimi-coding-text"],
        "Anthropic-protocol residuals are intentionally not wired in B6k"
    );
    let model = &models[0];
    assert_eq!(model.display_name, "Kimi Coding A");
    assert_eq!(model.context_window, Some(262_144));
    assert_eq!(model.supported_efforts, ["low", "high"]);
    let flags = model.extensions.as_ref().expect("Kimi flags");
    assert_eq!(flags.protocol, "openai");
    assert!(flags.supports_reasoning);
    assert!(flags.supports_vision);
    assert!(flags.supports_tool_use);
    assert!(flags.supports_thinking_type);
}

/// MUTATION CHECK: serialize the new Kimi extension field as null on old
/// catalog rows. Expected RUNTIME failure: the pinned pre-B6k cache row gains
/// bytes and older persisted catalogs churn.
#[test]
fn legacy_catalog_rows_serialize_byte_identically() {
    let row = DiscoveredModel {
        slug: "legacy".into(),
        display_name: "Legacy".into(),
        context_window: None,
        description: None,
        default_effort: None,
        supported_efforts: Vec::new(),
        visible: true,
        priority: None,
        extensions: None,
    };
    assert_eq!(
        serde_json::to_string(&row).expect("serialize legacy row"),
        r#"{"slug":"legacy","display_name":"Legacy","context_window":null,"description":null,"default_effort":null,"supported_efforts":[],"visible":true,"priority":null}"#
    );
}

/// The display fallback: a provider that names no display_name shows its
/// slug rather than an empty label.
#[test]
fn display_name_falls_back_to_the_slug() {
    let models = parse_catalog(
        CatalogSource::AnthropicSubscription,
        &serde_json::json!({"data": [{"id": "bare-slug"}]}),
    )
    .expect("parses");
    assert_eq!(
        models,
        vec![DiscoveredModel {
            slug: "bare-slug".into(),
            display_name: "bare-slug".into(),
            context_window: None,
            description: None,
            default_effort: None,
            supported_efforts: Vec::new(),
            visible: true,
            priority: None,
            extensions: None,
        }]
    );
}

/// MUTATION CHECK (W5f-2d): drop the `client_version` query param from
/// `catalog_request_url` for the OpenAI subscription source. Expected
/// runtime failure: the URL no longer carries `client_version`, which is a
/// hard 400 against the live codex endpoint (`missing field
/// 'client_version'`, confirmed 2026-07-30).
/// Verified by revert on 2026-07-30.
#[test]
fn the_openai_models_request_carries_client_version() {
    let url = haider_provider::catalog_request_url(
        CatalogSource::OpenAiSubscription,
        "https://chatgpt.com/backend-api/codex/models",
    );
    assert!(
        url.contains("client_version="),
        "the codex models request must carry client_version: {url}"
    );
    // Anthropic's endpoint takes no such param.
    let anthropic = haider_provider::catalog_request_url(
        CatalogSource::AnthropicSubscription,
        "https://api.anthropic.com/v1/models",
    );
    assert!(
        !anthropic.contains('?'),
        "the Anthropic request adds no query: {anthropic}"
    );
}
