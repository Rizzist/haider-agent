//! W5e-2 discovery: parse BOTH providers' real shapes, honour the
//! provider's own visibility/priority, and never synthesize a list.
//!
//! The OpenAI fixture is the SHAPE captured from the installed codex CLI's
//! live `~/.codex/models_cache.json` on 2026-07-30 (slugs deliberately
//! neutralized — this suite must never assert that a particular model
//! exists, only that whatever the provider names round-trips faithfully).
#![allow(clippy::expect_used)]

use haider_provider::{CatalogSource, DiscoveredModel, parse_catalog, pickable};

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

/// MUTATION CHECK: make `visible` unconditionally `true` (drop the
/// `visibility == "list"` test). Expected runtime failure: the hidden model
/// appears in `pickable` below.
/// Verified by revert on 2026-07-30.
#[test]
fn provider_visibility_and_priority_drive_the_picker() {
    let models = parse_catalog(CatalogSource::OpenAiSubscription, &codex_payload())
        .expect("parses");
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
            description: None,
            default_effort: None,
            supported_efforts: Vec::new(),
            visible: true,
            priority: None,
        }]
    );
}
