#![allow(clippy::expect_used)]

//! Laws for the ONE model-selection authority (F1).
//!
//! Sessions are provider-agnostic: every law here speaks model selection.
//! The live-session switch (`session.select_model`) and the spawn selector
//! resolve through the same functions, so these unit laws bind both.

use super::{ModelSelectionAuthority, SelectionRefusal, normalize_model_selector};
use haider_rpc::{
    ModelDetailWire, ModelInventoryAuthorityWire, ModelInventoryStatusWire, ProviderApiFamilyWire,
    ProviderAvailabilityWire, ProviderSummaryWire,
};
use std::collections::BTreeSet;

fn summary(
    provider: &str,
    models: &[&str],
    availability: ProviderAvailabilityWire,
) -> ProviderSummaryWire {
    ProviderSummaryWire {
        catalog_scope: None,
        configured_models: Vec::new(),
        static_models: Vec::new(),
        provider: provider.to_owned(),
        api_family: ProviderApiFamilyWire::OpenAiResponses,
        endpoint: None,
        response_open_timeout_ms: None,
        chunk_idle_timeout_ms: None,
        semantic_progress_timeout_ms: None,
        models: models.iter().map(|model| (*model).to_owned()).collect(),
        model_details: Vec::new(),
        catalog: haider_rpc::ProviderCatalogKindWire::Unknown,
        inventory: haider_rpc::ModelInventoryWire::Static,
        inventory_authority: ModelInventoryAuthorityWire::Authoritative,
        auth_methods: Vec::new(),
        availability,
        availability_reason: None,
        default_model: None,
        enabled: true,
        trust: haider_rpc::ProviderTrustWire::Full,
    }
}

fn creatable(providers: &[&str]) -> Option<BTreeSet<String>> {
    Some(
        providers
            .iter()
            .map(|provider| (*provider).to_owned())
            .collect(),
    )
}

fn authority(
    creatable_providers: &[&str],
    summaries: Vec<ProviderSummaryWire>,
) -> ModelSelectionAuthority {
    ModelSelectionAuthority::new(creatable(creatable_providers), Some(summaries))
}

#[test]
fn selection_derives_limits_when_inventory_details_are_incomplete() {
    let mut anthropic = summary(
        "anthropic-oauth",
        &["claude-fable-5-1"],
        ProviderAvailabilityWire::Available,
    );
    anthropic.api_family = ProviderApiFamilyWire::AnthropicMessages;
    let selection = authority(&["anthropic-oauth"], vec![anthropic])
        .validate_selection_with_status("anthropic-oauth", None, "claude-fable-5-1")
        .expect("known model selection");
    assert_eq!(selection.context_window, Some(1_000_000));
    assert_eq!(selection.max_output_tokens, 128_000);

    let selection = ModelSelectionAuthority::new(creatable(&["custom"]), None)
        .validate_selection_with_status("custom", None, "future-model")
        .expect("unknown inventory permits passthrough");
    assert!(selection.max_output_tokens > 4_096);
}

// ───────────────────────────── live-session selection ───────────────────────

/// LAW (absent_provider_keeps_legacy_bytes_and_behavior, behavior half): an
/// absent provider selects within the session's CURRENT provider even when
/// another provider also serves the model — nothing may guess cross-provider.
#[test]
fn absent_provider_selects_within_the_current_provider() {
    let authority = authority(
        &["openai", "anthropic-oauth"],
        vec![
            summary(
                "openai",
                &["gpt-a", "shared-model"],
                ProviderAvailabilityWire::Available,
            ),
            summary(
                "anthropic-oauth",
                &["shared-model"],
                ProviderAvailabilityWire::Available,
            ),
        ],
    );
    assert_eq!(
        authority.validate_selection("openai", None, "shared-model"),
        Ok(("openai".to_owned(), "shared-model".to_owned()))
    );
}

/// LAW (unavailable_provider_refused_typed): a row whose provider attribute
/// is not creatable is a typed refusal, and the copy speaks model selection.
#[test]
fn uncreatable_provider_is_refused_typed() {
    let authority = ModelSelectionAuthority::new(creatable(&["openai"]), None);
    let refusal = authority
        .validate_selection("openai", Some("frontier-imaginary"), "some-model")
        .expect_err("uncreatable provider must refuse");
    assert_eq!(
        refusal,
        SelectionRefusal::ProviderUnavailable {
            provider: "frontier-imaginary".to_owned(),
            reason: None,
        }
    );
    assert_eq!(refusal.kind(), "provider_unavailable");
    assert!(refusal.message().contains("model row"));
}

/// LAW (unknown_model_with_known_inventory_refused_typed): a provider with a
/// discovered inventory refuses a model outside it.
#[test]
fn unknown_model_with_known_inventory_is_refused_typed() {
    let authority = authority(
        &["openai", "anthropic-oauth"],
        vec![summary(
            "anthropic-oauth",
            &["fable-5", "fable-4.5"],
            ProviderAvailabilityWire::Available,
        )],
    );
    let refusal = authority
        .validate_selection("openai", Some("anthropic-oauth"), "fable-9-imaginary")
        .expect_err("model outside a known inventory must refuse");
    assert_eq!(
        refusal,
        SelectionRefusal::ModelUnknown {
            provider: "anthropic-oauth".to_owned(),
            model: "fable-9-imaginary".to_owned(),
            inventory_age_ms: None,
            suggestions: vec![
                "fable-4.5 · anthropic-oauth".to_owned(),
                "fable-5 · anthropic-oauth".to_owned(),
            ],
        }
    );
    assert_eq!(refusal.kind(), "model_unknown");
}

/// OWNER LAW (0.0.962 compatibility): built-in catalogs remain
/// authoritative, while a custom compatible catalog is advisory after the
/// same miss and reports `Unlisted` without fabricating an available row.
///
/// MUTATION CHECK: make every non-empty catalog authoritative or make every
/// catalog advisory. Expected RUNTIME failure: one of the paired decisions
/// below flips; appending the passthrough id to `models` flips the no-fabricate
/// assertion.
#[test]
fn built_in_inventory_rejects_the_same_miss_custom_inventory_admits_as_unlisted() {
    let built_in = summary(
        "openai",
        &["catalog-model"],
        ProviderAvailabilityWire::Available,
    );
    let mut custom = summary(
        "bench-proxy",
        &["catalog-model"],
        ProviderAvailabilityWire::Available,
    );
    custom.api_family = ProviderApiFamilyWire::OpenAiChatCompletions;
    custom.inventory_authority = ModelInventoryAuthorityWire::Advisory;
    let authority = authority(&["openai"], vec![built_in, custom.clone()]);

    assert!(matches!(
        authority.validate_selection_with_status("openai", None, "passthrough-model"),
        Err(SelectionRefusal::ModelUnknown { provider, model, .. })
            if provider == "openai" && model == "passthrough-model"
    ));
    let admitted = authority
        .validate_selection_with_status("openai", Some("bench-proxy"), "passthrough-model")
        .expect("custom advisory miss remains admissible");
    assert_eq!(admitted.provider, "bench-proxy");
    assert_eq!(admitted.model, "passthrough-model");
    assert_eq!(
        admitted.inventory_status,
        ModelInventoryStatusWire::Unlisted
    );
    assert_eq!(custom.models, ["catalog-model"]);
    assert!(custom.model_details.is_empty());
}

/// MUTATION CHECK: dropping the provider summary fetch timestamp from the
/// refusal erases the cache-age coordinate clients need after refresh-on-miss.
#[test]
fn model_unknown_carries_inventory_age_when_fetch_time_is_known() {
    let mut fetched = summary(
        "custom-router",
        &["known-model"],
        ProviderAvailabilityWire::Available,
    );
    fetched.inventory = haider_rpc::ModelInventoryWire::Fetched { fetched_at_ms: 1 };
    let authority = authority(&["openai", "custom-router"], vec![fetched]);
    let refusal = authority
        .validate_selection("openai", Some("custom-router"), "new-model")
        .expect_err("unknown model");
    let SelectionRefusal::ModelUnknown {
        inventory_age_ms: Some(inventory_age_ms),
        ..
    } = refusal
    else {
        panic!("known fetch time must produce a typed inventory age");
    };
    assert!(inventory_age_ms > 0);
}

/// A provider WITHOUT a discovered inventory accepts honestly — provider
/// errors surface at turn time, never a guessed refusal.
#[test]
fn unknown_inventory_accepts_honestly() {
    let authority = ModelSelectionAuthority::new(creatable(&["openai", "anthropic-oauth"]), None);
    assert_eq!(
        authority.validate_selection("openai", Some("anthropic-oauth"), "fable-5"),
        Ok(("anthropic-oauth".to_owned(), "fable-5".to_owned()))
    );
}

/// Selecting a row on the session's CURRENT provider never consults
/// creatability — the session already runs it (explicit == absent).
#[test]
fn current_provider_rows_skip_creatability() {
    let authority = ModelSelectionAuthority::new(None, None);
    assert_eq!(
        authority.validate_selection("fake", Some("fake"), "fake-model-2"),
        Ok(("fake".to_owned(), "fake-model-2".to_owned()))
    );
    assert_eq!(
        authority.validate_selection("fake", None, "fake-model-2"),
        Ok(("fake".to_owned(), "fake-model-2".to_owned()))
    );
}

#[test]
fn missing_provider_in_a_registered_snapshot_is_unavailable() {
    let healthy = summary("healthy", &["m-one"], ProviderAvailabilityWire::Available);
    for rows in [Vec::new(), vec![healthy.clone()]] {
        // Even a factory declaration cannot supply a row removed from an
        // installed management registry. Test empty and nonempty registries.
        let authority = ModelSelectionAuthority::new(creatable(&["ghost", "healthy"]), Some(rows));
        for requested in [None, Some("ghost")] {
            assert!(matches!(
                authority.validate_selection("ghost", requested, "m-one"),
                Err(SelectionRefusal::ProviderUnavailable { provider, .. }) if provider == "ghost"
            ));
        }
        for (model, provider) in [(None, None), (Some("m-one"), Some("ghost"))] {
            assert!(matches!(
                authority.resolve_child_selector("ghost", "m-one", model, provider),
                Err(SelectionRefusal::ProviderUnavailable { provider, .. }) if provider == "ghost"
            ));
        }
    }
    // A factory-only caller has no management registry to prove removal.
    let factory = ModelSelectionAuthority::new(creatable(&["ghost"]), None);
    assert!(factory.validate_selection("ghost", None, "m-one").is_ok());
    assert!(
        factory
            .validate_selection("other", Some("ghost"), "m-one")
            .is_ok()
    );
    assert_eq!(
        factory.resolve_child_selector("ghost", "m-one", None, None),
        Ok(("ghost".into(), "m-one".into()))
    );
    // An existing provider with unknown inventory remains distinct from a
    // missing provider, and a removed parent can select a healthy target.
    let mut unknown = summary("ghost", &[], ProviderAvailabilityWire::Available);
    unknown.inventory = haider_rpc::ModelInventoryWire::NeverFetched;
    unknown.catalog = haider_rpc::ProviderCatalogKindWire::Adapter;
    let present = ModelSelectionAuthority::new(None, Some(vec![unknown]));
    assert!(present.validate_selection("ghost", None, "m-one").is_ok());
    assert!(
        present
            .resolve_child_selector("ghost", "m-one", None, None)
            .is_ok()
    );
    let rescue = ModelSelectionAuthority::new(creatable(&["healthy"]), Some(vec![healthy]));
    assert_eq!(
        rescue.validate_selection("ghost", Some("healthy"), "m-one"),
        Ok(("healthy".into(), "m-one".into()))
    );
}

#[test]
fn child_selection_filters_unavailable_parent_inventory() {
    for disabled in [false, true] {
        let mut parent = summary(
            "parent",
            &["target"],
            if disabled {
                ProviderAvailabilityWire::Available
            } else {
                ProviderAvailabilityWire::Unavailable
            },
        );
        parent.enabled = !disabled;
        let healthy = summary("healthy", &["target"], ProviderAvailabilityWire::Available);
        let authority = ModelSelectionAuthority::new(
            creatable(&["parent", "healthy"]),
            Some(vec![parent.clone(), healthy]),
        );
        for model in ["target", "tar-get"] {
            assert_eq!(
                authority.resolve_child_selector("parent", "target", Some(model), None),
                Ok(("healthy".into(), "target".into()))
            );
        }
        assert_eq!(
            authority.resolve_child_selector("parent", "target", None, None),
            Ok(("parent".into(), "target".into()))
        );
        let alone = ModelSelectionAuthority::new(creatable(&["parent"]), Some(vec![parent]));
        assert!(matches!(
            alone.resolve_child_selector("parent", "target", Some("target"), None),
            Err(SelectionRefusal::ModelNotResolvable { .. })
        ));
    }
    let mut parent = summary("parent", &["target"], ProviderAvailabilityWire::Available);
    parent.inventory = haider_rpc::ModelInventoryWire::Stale {
        fetched_at_ms: 1,
        reason: "refresh due".into(),
    };
    let healthy = ModelSelectionAuthority::new(None, Some(vec![parent]));
    assert_eq!(
        healthy.resolve_child_selector("parent", "target", None, None),
        Ok(("parent".into(), "target".into()))
    );
}

/// Enabled custom OpenAI and Anthropic profiles are creatable without static
/// registry rows — the same rule `session.create` applies.
#[test]
fn enabled_custom_openai_and_anthropic_profiles_are_creatable() {
    for family in [
        ProviderApiFamilyWire::OpenAiChatCompletions,
        ProviderApiFamilyWire::AnthropicMessages,
    ] {
        let mut custom = summary(
            "my-endpoint",
            &["local-model"],
            ProviderAvailabilityWire::Available,
        );
        custom.api_family = family;
        let authority = ModelSelectionAuthority::new(creatable(&["openai"]), Some(vec![custom]));
        assert_eq!(
            authority.validate_selection("openai", Some("my-endpoint"), "local-model"),
            Ok(("my-endpoint".to_owned(), "local-model".to_owned()))
        );
    }
}

/// An empty model is a malformed selector, not a lookup.
#[test]
fn empty_model_is_an_invalid_selector() {
    let authority = ModelSelectionAuthority::new(creatable(&["openai"]), None);
    assert!(matches!(
        authority.validate_selection("openai", None, "  "),
        Err(SelectionRefusal::InvalidSelector { .. })
    ));
}

#[test]
fn model_selector_normalization_is_separator_insensitive_and_unicode_safe() {
    for spelling in [
        "glm-4.7-flashx",
        "GLM_4 7.FLASHX",
        "glm4.7 flashx",
        " glm 4_7-flashx ",
    ] {
        assert_eq!(normalize_model_selector(spelling), "glm47flashx");
    }
    assert_eq!(
        normalize_model_selector("MÖDEL.東京 Δ"),
        normalize_model_selector("mödel_東京-δ")
    );
}

#[test]
fn list_models_filters_model_provider_and_alias_and_reports_truncation() {
    let mut first = summary(
        "haider-code",
        &["glm-4.7-flashx", "glm-4.7-air"],
        ProviderAvailabilityWire::Available,
    );
    first.inventory = haider_rpc::ModelInventoryWire::Fetched { fetched_at_ms: 900 };
    first.model_details = vec![
        ModelDetailWire {
            source: None,
            name: "glm-4.7-flashx".into(),
            display_name: Some("ZAI Flash X".into()),
            context_window: Some(128_000),
            max_output_tokens: Some(32_768),
            max_output_tokens_source: None,
            supported_efforts: Vec::new(),
            default_effort: None,
            supported_speeds: vec!["fast".into()],
            supports_thinking_type: None,
            supports_vision: Some(true),
        },
        ModelDetailWire {
            source: None,
            name: "glm-4.7-air".into(),
            display_name: None,
            context_window: None,
            max_output_tokens: Some(8_192),
            max_output_tokens_source: None,
            supported_efforts: Vec::new(),
            default_effort: None,
            supported_speeds: Vec::new(),
            supports_thinking_type: None,
            supports_vision: Some(false),
        },
    ];
    let authority = authority(&["haider-code"], vec![first]);

    let alias_page = authority.model_catalog_at(Some("zai flash"), 100, 1_000);
    assert_eq!(alias_page.models.len(), 1);
    let row = &alias_page.models[0];
    assert_eq!(row.model, "glm-4.7-flashx");
    assert_eq!(row.provider, "haider-code");
    assert_eq!(row.inventory_age_ms, Some(100));
    assert_eq!(row.aliases, ["ZAI Flash X"]);
    assert_eq!(row.capabilities.vision, Some(true));
    assert!(row.capabilities.fast);
    assert!(row.capabilities.pdf);
    assert_eq!(row.capabilities.context_window, Some(128_000));
    assert_eq!(row.capabilities.max_output_tokens, 32_768);

    assert_eq!(
        authority
            .model_catalog_at(Some("HAIDER-CODE"), 100, 1_000)
            .models
            .len(),
        2
    );
    let truncated = authority.model_catalog_at(None, 1, 1_000);
    assert_eq!(truncated.models.len(), 1);
    assert!(truncated.truncated);
    assert!(
        truncated
            .hint
            .as_deref()
            .is_some_and(|hint| hint.contains("call list_models") && hint.contains("filter"))
    );
}

// ───────────────────────────── child spawn selector ─────────────────────────

/// LAW (child_inherits_the_parents_current_pair_by_default): no selector →
/// the parent's CURRENT pair verbatim. The runtime half — inheritance after
/// a mid-session pair switch — is pinned in `pair_switch_runtime_tests.rs`.
#[test]
fn absent_selector_inherits_the_parents_current_pair() {
    let authority = ModelSelectionAuthority::new(creatable(&["openai"]), None);
    assert_eq!(
        authority.resolve_child_selector("anthropic-oauth", "fable-5", None, None),
        Ok(("anthropic-oauth".to_owned(), "fable-5".to_owned()))
    );
}

/// LAW (preference order): the parent's own provider wins whenever its known
/// inventory serves the model — even when another available provider also
/// serves it. MUTATION: invert the preference (candidates before parent) and
/// this pins the child to `other`.
#[test]
fn bare_model_prefers_the_parents_provider() {
    let authority = authority(
        &["openai", "other"],
        vec![
            summary(
                "openai",
                &["shared-model"],
                ProviderAvailabilityWire::Available,
            ),
            summary(
                "other",
                &["shared-model"],
                ProviderAvailabilityWire::Available,
            ),
        ],
    );
    assert_eq!(
        authority.resolve_child_selector("openai", "gpt-a", Some("shared-model"), None),
        Ok(("openai".to_owned(), "shared-model".to_owned()))
    );
}

/// Exactly one available provider serving the model resolves without an
/// explicit provider.
#[test]
fn bare_model_resolves_through_the_single_serving_provider() {
    let authority = authority(
        &["openai", "anthropic-oauth"],
        vec![
            summary("openai", &["gpt-a"], ProviderAvailabilityWire::Available),
            summary(
                "anthropic-oauth",
                &["fable-5"],
                ProviderAvailabilityWire::Available,
            ),
        ],
    );
    assert_eq!(
        authority.resolve_child_selector("openai", "gpt-a", Some("fable-5"), None),
        Ok(("anthropic-oauth".to_owned(), "fable-5".to_owned()))
    );
}

#[test]
fn normalized_bare_model_resolves_to_the_single_canonical_catalog_slug() {
    let authority = authority(
        &["openai", "haider-code"],
        vec![
            summary("openai", &["gpt-a"], ProviderAvailabilityWire::Available),
            summary(
                "haider-code",
                &["glm-4.7-flashx"],
                ProviderAvailabilityWire::Available,
            ),
        ],
    );
    assert_eq!(
        authority.resolve_child_selector("openai", "gpt-a", Some("glm4.7 flashx"), None),
        Ok(("haider-code".to_owned(), "glm-4.7-flashx".to_owned()))
    );
}

#[test]
fn normalized_explicit_pair_resolves_to_the_canonical_catalog_slug() {
    let authority = authority(
        &["openai", "haider-code"],
        vec![
            summary("openai", &["gpt-a"], ProviderAvailabilityWire::Available),
            summary(
                "haider-code",
                &["glm-4.7-flashx"],
                ProviderAvailabilityWire::Available,
            ),
        ],
    );
    assert_eq!(
        authority.resolve_child_selector(
            "openai",
            "gpt-a",
            Some("GLM_4 7.FLASHX"),
            Some("haider-code"),
        ),
        Ok(("haider-code".to_owned(), "glm-4.7-flashx".to_owned()))
    );
}

#[test]
fn literal_exact_model_wins_before_a_normalized_collision() {
    let authority = authority(
        &["openai", "other"],
        vec![
            summary(
                "openai",
                &["glm_4.7_flashx"],
                ProviderAvailabilityWire::Available,
            ),
            summary(
                "other",
                &["glm-4.7-flashx"],
                ProviderAvailabilityWire::Available,
            ),
        ],
    );
    assert_eq!(
        authority.resolve_child_selector("openai", "gpt-a", Some("glm-4.7-flashx"), None),
        Ok(("other".to_owned(), "glm-4.7-flashx".to_owned()))
    );
}

/// LAW (ambiguous_model_is_typed_with_candidates): several serving providers
/// refuse typed, NAMING every candidate — never a guess.
#[test]
fn ambiguous_bare_model_is_typed_with_candidates() {
    let authority = authority(
        &["openai", "kimi", "other"],
        vec![
            summary("openai", &["gpt-a"], ProviderAvailabilityWire::Available),
            summary(
                "kimi",
                &["shared-model"],
                ProviderAvailabilityWire::Available,
            ),
            summary(
                "other",
                &["shared-model"],
                ProviderAvailabilityWire::Available,
            ),
        ],
    );
    let refusal = authority
        .resolve_child_selector("openai", "gpt-a", Some("shared-model"), None)
        .expect_err("two candidates must not be guessed between");
    let SelectionRefusal::ModelNotResolvable {
        model, candidates, ..
    } = &refusal
    else {
        panic!("expected ModelNotResolvable, got {refusal:?}");
    };
    assert_eq!(model, "shared-model");
    assert_eq!(candidates, &["kimi".to_owned(), "other".to_owned()]);
    assert!(refusal.message().contains("kimi"));
    assert!(refusal.message().contains("other"));
}

/// LAW (unavailable_is_typed): nobody serves the bare model → typed refusal
/// with EMPTY candidates and bounded catalog guidance.
#[test]
fn unserved_bare_model_is_typed_with_empty_candidates() {
    let authority = authority(
        &["openai"],
        vec![summary(
            "openai",
            &["gpt-a"],
            ProviderAvailabilityWire::Available,
        )],
    );
    let refusal = authority
        .resolve_child_selector("openai", "gpt-a", Some("nobody-serves-this"), None)
        .expect_err("unserved model must refuse");
    assert_eq!(
        refusal,
        SelectionRefusal::ModelNotResolvable {
            model: "nobody-serves-this".to_owned(),
            candidates: Vec::new(),
            suggestions: vec!["gpt-a · openai".to_owned()],
        }
    );
    assert!(refusal.message().contains("call list_models"));
    assert_eq!(
        refusal.details()["suggestions"],
        serde_json::json!(["gpt-a · openai"])
    );
}

#[test]
fn near_match_ranking_is_nearest_first_capped_and_empty_inventory_safe() {
    let ranked_authority = authority(
        &["haider-code"],
        vec![summary(
            "haider-code",
            &[
                "glm-4.7-flash",
                "gpt-5.6",
                "glm-4.7-flashx",
                "glm-4.6-flashx",
                "glm-4.7-air",
                "glm-4.5-air",
                "other-model",
            ],
            ProviderAvailabilityWire::Available,
        )],
    );
    let refusal = ranked_authority
        .resolve_child_selector("openai", "gpt-a", Some("glm4.7 flshx"), None)
        .expect_err("typo must refuse with suggestions");
    let SelectionRefusal::ModelNotResolvable { suggestions, .. } = refusal else {
        panic!("expected model_not_resolvable")
    };
    assert_eq!(suggestions.len(), 5);
    assert_eq!(suggestions[0], "glm-4.7-flashx · haider-code");

    let empty = ModelSelectionAuthority::new(creatable(&["openai"]), None)
        .resolve_child_selector("openai", "gpt-a", Some("anything"), None)
        .expect_err("empty inventory refuses honestly");
    assert!(matches!(
        empty,
        SelectionRefusal::ModelNotResolvable { suggestions, .. } if suggestions.is_empty()
    ));
}

#[test]
fn bare_suggestions_can_name_an_unconfigured_row_without_auto_selecting_it() {
    let authority = authority(
        &["openai"],
        vec![
            summary("openai", &["gpt-a"], ProviderAvailabilityWire::Available),
            summary(
                "not-configured",
                &["glm-4.7-flashx"],
                ProviderAvailabilityWire::Available,
            ),
        ],
    );
    let refusal = authority
        .resolve_child_selector("openai", "gpt-a", Some("glm4.7 flashx"), None)
        .expect_err("an unconfigured provider must never be auto-selected");
    assert_eq!(
        refusal,
        SelectionRefusal::ModelNotResolvable {
            model: "glm4.7 flashx".into(),
            candidates: Vec::new(),
            suggestions: vec![
                "glm-4.7-flashx · not-configured".into(),
                "gpt-a · openai".into()
            ],
        }
    );
}

#[test]
fn explicit_unknown_slug_suggestions_are_scoped_to_its_provider() {
    let authority = authority(
        &["fake-a", "fake-b"],
        vec![
            summary(
                "fake-a",
                &["glm-4.7-flash"],
                ProviderAvailabilityWire::Available,
            ),
            summary(
                "fake-b",
                &["glm-4.7-flashx"],
                ProviderAvailabilityWire::Available,
            ),
        ],
    );
    let refusal = authority
        .resolve_child_selector("fake-a", "model-a", Some("glm-4.7-flashz"), Some("fake-b"))
        .expect_err("unknown explicit slug");
    let SelectionRefusal::ModelUnknown { suggestions, .. } = refusal else {
        panic!("expected model_unknown")
    };
    assert_eq!(suggestions, ["glm-4.7-flashx · fake-b"]);
}

/// An UNAVAILABLE provider serving the model is not a candidate: with no
/// other server the bare model refuses instead of landing on a dead row.
#[test]
fn unavailable_providers_are_not_candidates() {
    let authority = authority(
        &["openai", "kimi"],
        vec![
            summary("openai", &["gpt-a"], ProviderAvailabilityWire::Available),
            summary(
                "kimi",
                &["shared-model"],
                ProviderAvailabilityWire::Unavailable,
            ),
        ],
    );
    assert!(matches!(
        authority.resolve_child_selector("openai", "gpt-a", Some("shared-model"), None),
        Err(SelectionRefusal::ModelNotResolvable { candidates, .. }) if candidates.is_empty()
    ));
}

/// An explicit pair rides the SAME validation as a live-session selection:
/// cross-provider works when creatable, and a known inventory still binds.
#[test]
fn explicit_pair_validates_like_a_live_selection() {
    let authority = authority(
        &["fake-a", "fake-b"],
        vec![summary(
            "fake-b",
            &["model-b"],
            ProviderAvailabilityWire::Available,
        )],
    );
    assert_eq!(
        authority.resolve_child_selector("fake-a", "model-a", Some("model-b"), Some("fake-b")),
        Ok(("fake-b".to_owned(), "model-b".to_owned()))
    );
    assert!(matches!(
        authority.resolve_child_selector("fake-a", "model-a", Some("model-x"), Some("fake-b")),
        Err(SelectionRefusal::ModelUnknown { .. })
    ));
    assert!(matches!(
        authority.resolve_child_selector("fake-a", "model-a", Some("model-c"), Some("fake-c")),
        Err(SelectionRefusal::ProviderUnavailable { .. })
    ));
}

/// A provider without a model is a malformed selector — the selector is the
/// MODEL; the provider only disambiguates it.
#[test]
fn provider_without_model_is_an_invalid_selector() {
    let authority = ModelSelectionAuthority::new(creatable(&["openai"]), None);
    assert!(matches!(
        authority.resolve_child_selector("openai", "gpt-a", None, Some("openai")),
        Err(SelectionRefusal::InvalidSelector { .. })
    ));
}

// ───────────────────────────── G4b enterprise tuning ────────────────────────

/// LAW (LE-x, gate halves): `/effort` validates on bedrock/vertex pairs
/// through the normalized static tables — `anthropic.claude-opus-5` and a
/// dated vertex slug resolve their family ladders even with NO management
/// snapshot — while `/fast` refuses on the `bedrock`/`vertex` provider ids
/// REGARDLESS of model, and keeps accepting the same models on the
/// first-party anthropic providers (both directions).
///
/// MUTATION CHECK: drop bedrock/vertex from `effort_ladder`'s static arm
/// (the effort half), or widen `validate_fast`'s provider match to include
/// them (the fast half). Expected RUNTIME failure: the named assertions.
#[test]
fn le_bedrock_and_vertex_pairs_validate_effort_but_refuse_fast() {
    let authority =
        ModelSelectionAuthority::new(creatable(&["bedrock", "vertex", "anthropic"]), None);
    authority
        .validate_effort("bedrock", "anthropic.claude-opus-5", Some("xhigh"))
        .expect("normalized bedrock spelling resolves the opus-5 ladder");
    authority
        .validate_effort("vertex", "claude-sonnet-4-6@20251101", Some("max"))
        .expect("dated vertex slug resolves the sonnet-4-6 ladder");
    assert!(
        authority
            .validate_effort("vertex", "claude-sonnet-4-6@20251101", Some("xhigh"))
            .is_err(),
        "the 4.6 ladder has no xhigh — normalization never widens it"
    );
    assert!(
        authority
            .validate_effort("bedrock", "anthropic.claude-nova-1", Some("high"))
            .is_err(),
        "unknown families keep the honest empty ladder"
    );

    for (provider, model) in [
        ("bedrock", "anthropic.claude-opus-5"),
        ("bedrock", "claude-opus-5"),
        ("vertex", "claude-opus-5"),
        ("vertex", "claude-opus-4-8@20260115"),
    ] {
        assert!(
            authority.validate_fast(provider, model, true).is_err(),
            "fast must refuse on {provider} · {model}"
        );
        authority
            .validate_fast(provider, model, false)
            .expect("disabling fast is always accepted — recovery is never gated");
    }
    authority
        .validate_fast("anthropic", "claude-opus-5", true)
        .expect("the first-party pair keeps fast");
    authority
        .validate_fast("anthropic", "anthropic.claude-opus-5", true)
        .expect("normalization admits the enterprise spelling ON the claude api");
}

#[test]
fn remote_inventory_cannot_admit_an_unresolved_model_after_failed_discovery() {
    for inventory in [
        haider_rpc::ModelInventoryWire::NeverFetched,
        haider_rpc::ModelInventoryWire::Unavailable {
            reason: "offline".into(),
        },
    ] {
        let mut remote = summary("haider-code", &[], ProviderAvailabilityWire::Unavailable);
        remote.inventory = inventory;
        remote.catalog = haider_rpc::ProviderCatalogKindWire::Public;
        let authority = authority(&["haider-code"], vec![remote]);
        assert!(matches!(
            authority.validate_selection("haider-code", None, "Go"),
            Err(SelectionRefusal::ProviderUnavailable { .. })
        ));
    }
    let mut remote = summary(
        "haider-code",
        &["deepseek-v4-flash"],
        ProviderAvailabilityWire::Available,
    );
    remote.catalog = haider_rpc::ProviderCatalogKindWire::Public;
    remote.inventory = haider_rpc::ModelInventoryWire::Fetched { fetched_at_ms: 1 };
    let authority = authority(&["haider-code"], vec![remote]);
    assert!(
        authority
            .validate_selection("haider-code", None, "Go")
            .is_err()
    );
    assert_eq!(
        authority
            .validate_selection("haider-code", None, "deepseek-v4-flash")
            .expect("real ID"),
        ("haider-code".into(), "deepseek-v4-flash".into())
    );
}

#[test]
fn static_subscription_row_is_selectable_after_catalog_403() {
    let mut subscription = summary(
        "anthropic-oauth",
        &["claude-fable-5-1"],
        ProviderAvailabilityWire::Available,
    );
    subscription.catalog = haider_rpc::ProviderCatalogKindWire::Authenticated;
    subscription.model_details = vec![ModelDetailWire {
        source: Some(haider_rpc::ModelDetailSourceWire::Static),
        name: "claude-fable-5-1".into(),
        display_name: None,
        context_window: Some(1_000_000),
        max_output_tokens: None,
        max_output_tokens_source: None,
        supported_efforts: Vec::new(),
        default_effort: None,
        supported_speeds: Vec::new(),
        supports_thinking_type: None,
        supports_vision: None,
    }];
    subscription.inventory = haider_rpc::ModelInventoryWire::Unavailable {
        reason: "provider does not serve a model list to this credential (403)".into(),
    };
    let authority = authority(&["anthropic-oauth"], vec![subscription]);
    assert_eq!(
        authority.validate_selection("anthropic-oauth", None, "claude-fable-5-1"),
        Ok(("anthropic-oauth".into(), "claude-fable-5-1".into()))
    );
    assert!(matches!(
        authority.validate_selection("anthropic-oauth", None, "invented-model"),
        Err(SelectionRefusal::ModelUnknown { .. })
    ));
}

#[test]
fn configured_azure_selection_reports_discovery_reason_and_accepts_unsupported_listing() {
    let mut azure = summary(
        "azure",
        &["my-deployment"],
        ProviderAvailabilityWire::Unavailable,
    );
    azure.endpoint = Some("https://synthetic.openai.azure.com/openai/v1".into());
    azure.inventory = haider_rpc::ModelInventoryWire::Configured;
    azure.inventory_authority = ModelInventoryAuthorityWire::Advisory;
    for reason in [
        "model list not fetched — fetch models before selecting",
        "server returned 503 for /models — retry model-list fetch",
        "model catalog timed out — retry model-list fetch",
    ] {
        azure.availability_reason = Some(reason.into());
        for current in ["azure", "other"] {
            let refusal = authority(&[], vec![azure.clone()])
                .validate_selection(current, Some("azure"), "my-deployment")
                .expect_err("unavailable deployment");
            assert!(refusal.message().contains(reason));
            assert_eq!(refusal.details()["reason"], reason);
            assert_eq!(refusal.kind(), "provider_unavailable");
        }
    }
    azure.availability = ProviderAvailabilityWire::Available;
    azure.availability_reason =
        Some("configured (unverified): server returned 404 for /models".into());
    for current in ["azure", "other"] {
        assert!(
            authority(&["azure"], vec![azure.clone()])
                .validate_selection(current, Some("azure"), "my-deployment")
                .is_ok()
        );
    }
    azure.endpoint = Some("http://127.0.0.1:18123/v1".into());
    azure.availability = ProviderAvailabilityWire::Unavailable;
    assert!(
        authority(&[], vec![azure])
            .validate_selection("azure", None, "my-deployment")
            .is_err(),
        "all configured providers require the same discovery evidence"
    );
}

#[test]
fn permanent_ids_do_not_inherit_their_remote_default_settings_as_unowned_configuration() {
    let mut profile = summary(
        "custom",
        &["my-deployment"],
        ProviderAvailabilityWire::Available,
    );
    profile.configured_models = vec!["my-deployment".into()];
    profile.catalog_scope = Some(haider_rpc::ProviderCatalogScopeWire {
        key: "synthetic-account-A".into(),
        account_alias: Some("work".into()),
        account_incarnation: Some(1),
    });
    profile.model_details = vec![ModelDetailWire {
        max_output_tokens_source: None,
        source: Some(haider_rpc::ModelDetailSourceWire::Remote),
        name: "my-deployment".into(),
        display_name: None,
        context_window: Some(100_000),
        max_output_tokens: Some(500),
        supported_efforts: Vec::new(),
        default_effort: None,
        supported_speeds: Vec::new(),
        supports_thinking_type: None,
        supports_vision: None,
    }];
    use haider_protocol::output_budget::SessionOutputBudgetSourceV1;
    let derived = SessionOutputBudgetSourceV1::Derived.apply(500, 500);
    let clamped = SessionOutputBudgetSourceV1::UserSet { requested: 1000 }.apply(500, 500);
    let explicit = SessionOutputBudgetSourceV1::UserSet { requested: 100 }.apply(500, 500);
    let selection = authority(&["custom"], vec![profile.clone()]);
    assert_eq!(
        selection.model_catalog_key("custom", "my-deployment"),
        None,
        "the declared ID is permanent"
    );
    assert_eq!(
        selection
            .model_settings_catalog_key("custom", "my-deployment", &derived)
            .as_deref(),
        Some("synthetic-account-A"),
        "the fetched output default is A's data"
    );
    assert_eq!(
        selection
            .model_settings_catalog_key("custom", "my-deployment", &clamped)
            .as_deref(),
        Some("synthetic-account-A"),
        "the effective ceiling is fetched even though the requested budget is user-set"
    );
    assert_eq!(
        selection.model_settings_catalog_key("custom", "my-deployment", &explicit),
        None,
        "an unchanged explicit budget remains user configuration"
    );
    profile.model_details[0].source = Some(haider_rpc::ModelDetailSourceWire::Configured);
    profile.model_details[0].max_output_tokens = None;
    let selection = authority(&["custom"], vec![profile]);
    assert_eq!(
        selection.model_settings_catalog_key("custom", "my-deployment", &derived),
        None,
        "ordinary configured fallback uses no fetched defaults"
    );
}

#[test]
fn selection_scope_follows_the_session_route_instead_of_borrowing_the_active_account() {
    for (route, retired) in [
        ("same", false),
        ("endpoint", true),
        ("account", true),
        ("legacy", false),
    ] {
        let mut profile = summary(
            "custom",
            &["my-deployment", "a-private-only"],
            ProviderAvailabilityWire::Available,
        );
        profile.catalog = haider_rpc::ProviderCatalogKindWire::Custom;
        profile.inventory = haider_rpc::ModelInventoryWire::Fetched { fetched_at_ms: 1 };
        profile.inventory_authority = ModelInventoryAuthorityWire::Advisory;
        profile.endpoint = Some("http://127.0.0.1:23456/v1".into());
        profile.configured_models = vec!["my-deployment".into()];
        profile.catalog_scope = (route != "legacy").then(|| haider_rpc::ProviderCatalogScopeWire {
            key: "synthetic-account-A".into(),
            account_alias: Some("work".into()),
            account_incarnation: Some(1),
        });
        profile.model_details = profile
            .models
            .iter()
            .map(|name| ModelDetailWire {
                max_output_tokens_source: None,
                source: Some(haider_rpc::ModelDetailSourceWire::Remote),
                name: name.clone(),
                display_name: Some("A's private label".into()),
                context_window: Some(100_000),
                max_output_tokens: Some(500),
                supported_efforts: vec!["high".into()],
                default_effort: Some("high".into()),
                supported_speeds: vec!["fast".into()],
                supports_thinking_type: Some(true),
                supports_vision: Some(false),
            })
            .collect();
        let current: haider_protocol::session::SessionMetadataV1 = serde_json::from_value(serde_json::json!({
            "cwd":"/synthetic", "provider":"custom", "model":"my-deployment", "max_tokens":500, "created_at_ms":1,
            "provider_base_url": if route == "endpoint" { "http://127.0.0.1:23457/v1" } else { "http://127.0.0.1:23456/v1" },
            "account_alias": if route == "account" { "other" } else { "work" },
        })).expect("typed session metadata");
        let mut selected =
            authority(&["custom"], vec![profile]).scoped_to_session_route(&current, "custom");
        if retired {
            assert!(
                matches!(
                    selected.validate_selection_with_status("custom", None, "my-deployment"),
                    Err(SelectionRefusal::ProviderUnavailable { .. })
                ),
                "a new route cannot borrow the previous route listing"
            );
            // Unsupported discovery on this route is independent positive
            // evidence for configured passthrough, without A's fetched data.
            selected.summaries[0].availability = ProviderAvailabilityWire::Available;
            selected.summaries[0].availability_reason =
                Some("configured (unverified): server returned 405 for /models".into());
        }
        let resolved = selected
            .validate_selection_with_status("custom", None, "my-deployment")
            .expect("matching list or route-owned unsupported evidence");
        if retired {
            assert!(!selected.catalog_key_is_current("custom", "synthetic-account-A"));
            assert_eq!(resolved.context_window, None);
            assert_ne!(resolved.max_output_tokens, 500);
            assert!(
                !selected.summaries[0]
                    .models
                    .iter()
                    .any(|name| name == "a-private-only")
            );
            let detail = &selected.summaries[0].model_details[0];
            assert_eq!(
                detail.source,
                Some(haider_rpc::ModelDetailSourceWire::Configured)
            );
            assert_eq!(detail.supports_vision, None);
            assert!(detail.supported_efforts.is_empty());
            let explicit = selected
                .validate_selection_with_status("custom", None, "manual-unlisted")
                .expect("advisory configuration is preserved");
            assert_eq!(explicit.context_window, None);
        } else {
            assert_eq!(resolved.context_window, Some(100_000));
            assert_eq!(resolved.max_output_tokens, 500);
        }
    }
}

#[test]
fn unsupported_azure_evidence_cannot_be_borrowed_by_a_rebound_account() {
    let mut azure = summary(
        "azure",
        &["my-deployment"],
        ProviderAvailabilityWire::Available,
    );
    azure.endpoint = Some("https://synthetic.openai.azure.com/openai/v1".into());
    azure.catalog = haider_rpc::ProviderCatalogKindWire::Custom;
    azure.inventory_authority = ModelInventoryAuthorityWire::Advisory;
    azure.inventory = haider_rpc::ModelInventoryWire::Unavailable {
        reason: "server returned 404 for /models".into(),
    };
    azure.configured_models = vec!["my-deployment".into()];
    azure.catalog_scope = Some(haider_rpc::ProviderCatalogScopeWire {
        key: "synthetic-account-A".into(),
        account_alias: Some("work".into()),
        account_incarnation: Some(1),
    });
    for (account, allowed) in [("work", true), ("other", false)] {
        let current = serde_json::from_value(serde_json::json!({
            "cwd":"/synthetic", "provider":"azure", "model":"my-deployment", "max_tokens":500, "created_at_ms":1,
            "account_alias":account,
        })).expect("session metadata");
        let selection = authority(&["azure"], vec![azure.clone()])
            .scoped_to_session_route(&current, "azure")
            .validate_selection_with_status("azure", None, "my-deployment");
        assert_eq!(selection.is_ok(), allowed);
        if !allowed {
            let refusal = selection.expect_err("B needs its own discovery evidence");
            assert!(!refusal.message().contains("404"));
            assert_eq!(refusal.kind(), "provider_unavailable");
        }
    }
}

#[test]
fn configured_selector_aliases_cannot_bypass_fresh_absence() {
    let mut profile = summary("custom", &["healthy"], ProviderAvailabilityWire::Available);
    profile.catalog = haider_rpc::ProviderCatalogKindWire::Custom;
    profile.inventory_authority = ModelInventoryAuthorityWire::Advisory;
    profile.inventory = haider_rpc::ModelInventoryWire::Fetched {
        fetched_at_ms: u64::MAX,
    };
    profile.configured_models = vec!["deployment".into(), "MÖDEL.東京 Δ".into(), "İ-model".into()];
    profile.configured_models.push("deployment".into());
    let absent = authority(&["custom"], vec![profile.clone()]);
    for model in [
        "deployment",
        "DEPLOYMENT",
        "de.ploy_ment",
        "de\u{a0}ploy\u{2003}ment",
        "mödel_東京-δ",
        "i\u{307}.mo_DEL",
    ] {
        assert!(
            matches!(
                absent.validate_selection("custom", None, model),
                Err(SelectionRefusal::ProviderUnavailable { .. })
            ),
            "{model}"
        );
        assert!(
            matches!(
                absent.resolve_child_selector("custom", "healthy", Some(model), Some("custom")),
                Err(SelectionRefusal::ProviderUnavailable { .. })
            ),
            "explicit child: {model}"
        );
    }
    assert_eq!(
        absent.validate_selection("custom", None, "HEALTHY"),
        Ok(("custom".into(), "healthy".into()))
    );
    assert_eq!(
        absent.validate_selection("custom", None, "future/model"),
        Ok(("custom".into(), "future/model".into()))
    );
    profile.inventory = haider_rpc::ModelInventoryWire::Stale {
        fetched_at_ms: 1,
        reason: "refresh due".into(),
    };
    let stale = authority(&["custom"], vec![profile]);
    assert_eq!(
        stale.validate_selection("custom", None, "DEPLOY.MENT"),
        Ok(("custom".into(), "deployment".into()))
    );
    assert_eq!(
        stale.validate_selection("custom", None, "mödel_東京-δ"),
        Ok(("custom".into(), "MÖDEL.東京 Δ".into()))
    );
}

fn c5_profile(model: &str) -> ProviderSummaryWire {
    let mut profile = summary("custom", &[model], ProviderAvailabilityWire::Available);
    profile.inventory_authority = ModelInventoryAuthorityWire::Advisory;
    profile.catalog_scope = Some(haider_rpc::ProviderCatalogScopeWire {
        key: "synthetic-owner-A".into(),
        account_alias: Some("work".into()),
        account_incarnation: Some(1),
    });
    profile.model_details = vec![ModelDetailWire {
        max_output_tokens_source: None,
        name: model.into(),
        source: Some(haider_rpc::ModelDetailSourceWire::Remote),
        context_window: Some(32_000),
        max_output_tokens: Some(2_048),
        supported_efforts: vec!["low".into(), "high".into()],
        display_name: None,
        default_effort: None,
        supported_speeds: Vec::new(),
        supports_thinking_type: None,
        supports_vision: None,
    }];
    profile
}

fn c5_assert_coordinate(model: &str, spelling: &str) {
    use haider_protocol::output_budget::SessionOutputBudgetSourceV1::{Derived, UserSet};
    let profile = c5_profile(model);
    let authority = authority(&["custom"], vec![profile]);
    let selected = authority
        .validate_selection_with_status("custom", None, spelling)
        .expect("unique selector");
    assert_eq!(selected.model, model);
    assert_eq!(selected.max_output_tokens, 2_048);
    assert_eq!(
        authority.model_catalog_key("custom", spelling).as_deref(),
        Some("synthetic-owner-A"),
        "C5 resolved model owns the selection: {spelling:?}"
    );
    for source in [Derived, UserSet { requested: 4_096 }] {
        assert_eq!(
            authority
                .model_settings_catalog_key("custom", spelling, &source.apply(2_048, 2_048))
                .as_deref(),
            Some("synthetic-owner-A"),
            "C5 resolved declaration owns derived/clamped settings: {spelling:?}"
        );
    }
    assert_eq!(
        authority.model_settings_catalog_key(
            "custom",
            spelling,
            &UserSet { requested: 1_024 }.apply(2_048, 2_048)
        ),
        None
    );
    assert!(
        authority
            .validate_effort("custom", spelling, Some("high"))
            .is_ok()
    );
    assert!(
        authority
            .validate_effort("custom", spelling, Some("max"))
            .is_err()
    );
}

macro_rules! c5_spelling {
    ($test:ident, $spelling:expr) => {
        #[test]
        fn $test() {
            c5_assert_coordinate("shared-model", $spelling);
        }
    };
}
c5_spelling!(c5_coordinate_case, "SHARED-MODEL");
c5_spelling!(c5_coordinate_underscore, "shared_model");
c5_spelling!(c5_coordinate_dot, "shared.model");
c5_spelling!(c5_coordinate_space, "shared model");
c5_spelling!(c5_coordinate_nbsp, "shared\u{a0}model");
c5_spelling!(c5_coordinate_emspace, "shared\u{2003}model");
c5_spelling!(
    c5_coordinate_mixed_separators,
    " Shared._-\u{a0}\u{2003}MODEL "
);

#[test]
fn c5_coordinate_scalar_lowercase_expansion() {
    c5_assert_coordinate("i\u{307}-k-ß", "İ.K_ẞ");
}

#[test]
fn c5_permanent_rows_keep_fetched_settings_owner_only() {
    use haider_protocol::output_budget::SessionOutputBudgetSourceV1::{Derived, UserSet};
    for configured in [false, true] {
        let mut profile = c5_profile("shared-model");
        if configured {
            profile.configured_models.push("shared-model".into());
        } else {
            profile.static_models.push("shared-model".into());
        }
        let authority = authority(&["custom"], vec![profile]);
        assert_eq!(authority.model_catalog_key("custom", "SHARED_MODEL"), None);
        for source in [Derived, UserSet { requested: 4_096 }] {
            assert_eq!(
                authority
                    .model_settings_catalog_key(
                        "custom",
                        "SHARED_MODEL",
                        &source.apply(2_048, 2_048)
                    )
                    .as_deref(),
                Some("synthetic-owner-A")
            );
        }
        assert_eq!(
            authority.model_settings_catalog_key(
                "custom",
                "SHARED_MODEL",
                &UserSet { requested: 1_024 }.apply(2_048, 2_048)
            ),
            None
        );
    }
}

#[test]
fn c5_ambiguous_unlisted_and_noncontract_unicode_remain_literal() {
    let mut profile = c5_profile("shared-model");
    profile.models.push("shared_model".into());
    let authority = authority(&["custom"], vec![profile]);
    for literal in [
        "SHARED.MODEL",
        "unlisted",
        "shared/model",
        "sharéd-model",
        "shared\u{2014}model",
    ] {
        assert_eq!(
            authority
                .validate_selection("custom", None, literal)
                .expect("advisory literal")
                .1,
            literal
        );
        assert_eq!(authority.model_catalog_key("custom", literal), None);
        assert!(
            authority
                .validate_effort("custom", literal, Some("high"))
                .is_err()
        );
    }
    assert_eq!(
        authority
            .model_catalog_key("custom", "shared-model")
            .as_deref(),
        Some("synthetic-owner-A"),
        "exact wins even in a normalized collision"
    );
}

macro_rules! c5_static_effort {
    ($test:ident, $provider:expr, $model:expr, $spelling:expr, $effort:expr, $fast:expr) => {
        #[test]
        fn $test() {
            let authority = authority(
                &[$provider],
                vec![summary(
                    $provider,
                    &[$model],
                    ProviderAvailabilityWire::Available,
                )],
            );
            for spelling in [$spelling, &format!("  {}  ", $model)] {
                assert!(
                    authority
                        .validate_effort($provider, spelling, Some($effort))
                        .is_ok(),
                    "C5 static effort for {spelling:?}"
                );
                assert_eq!(
                    authority.validate_fast($provider, spelling, true).is_ok(),
                    $fast,
                    "C5 fast gate for {spelling:?}"
                );
                assert!(authority.validate_fast($provider, spelling, false).is_ok());
            }
        }
    };
}
c5_static_effort!(
    c5_static_claude_effort_fast,
    "anthropic",
    "claude-opus-5",
    "CLAUDE_OPUS_5",
    "xhigh",
    true
);
c5_static_effort!(
    c5_static_oauth_claude_effort_fast,
    "anthropic-oauth",
    "claude-opus-4-8",
    "CLAUDE.OPUS.4.8",
    "max",
    true
);
c5_static_effort!(
    c5_static_gemini_effort,
    "gemini",
    "gemini-3.1-pro",
    "GEMINI 3_1 PRO",
    "high",
    false
);
c5_static_effort!(
    c5_static_bedrock_effort,
    "bedrock",
    "anthropic.claude-opus-5",
    "ANTHROPIC_CLAUDE_OPUS_5",
    "xhigh",
    false
);
c5_static_effort!(
    c5_static_vertex_effort,
    "vertex",
    "claude-sonnet-4-6@20251101",
    "CLAUDE_SONNET_4_6@20251101",
    "max",
    false
);

#[test]
fn c5_declared_effort_accepts_alternate_and_trimmed_creation_or_later_tuning() {
    let authority = authority(&["custom"], vec![c5_profile("shared-model")]);
    for spelling in [
        "SHARED_MODEL",
        " shared-model ",
        "shared\u{a0}model",
        "shared\u{2003}model",
    ] {
        assert!(
            authority
                .validate_effort("custom", spelling, Some("high"))
                .is_ok(),
            "C5 declared effort uses canonical row: {spelling:?}"
        );
    }
}

#[test]
fn c5_trimmed_static_capabilities_without_a_management_snapshot() {
    let authority = ModelSelectionAuthority::new(creatable(&["anthropic"]), None);
    assert!(
        authority
            .validate_effort("anthropic", "  claude-opus-5  ", Some("xhigh"))
            .is_ok(),
        "trimmed exact static effort must remain usable before management publication"
    );
    assert!(
        authority
            .validate_fast("anthropic", "  claude-opus-5  ", true)
            .is_ok(),
        "trimmed exact static fast must remain usable before management publication"
    );
}
