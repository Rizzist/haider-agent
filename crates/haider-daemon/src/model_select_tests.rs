#![allow(clippy::expect_used)]

//! Laws for the ONE model-selection authority (F1).
//!
//! Sessions are provider-agnostic: every law here speaks model selection.
//! The live-session switch (`session.select_model`) and the spawn selector
//! resolve through the same functions, so these unit laws bind both.

use super::{ModelSelectionAuthority, SelectionRefusal};
use haider_rpc::{ProviderApiFamilyWire, ProviderAvailabilityWire, ProviderSummaryWire};
use std::collections::BTreeSet;

fn summary(
    provider: &str,
    models: &[&str],
    availability: ProviderAvailabilityWire,
) -> ProviderSummaryWire {
    ProviderSummaryWire {
        provider: provider.to_owned(),
        api_family: ProviderApiFamilyWire::OpenAiResponses,
        endpoint: None,
        response_open_timeout_ms: None,
        models: models.iter().map(|model| (*model).to_owned()).collect(),
        model_details: Vec::new(),
        inventory_fetched_at_ms: None,
        auth_methods: Vec::new(),
        availability,
        availability_reason: None,
        default_model: None,
        enabled: true,
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
    ModelSelectionAuthority::new(creatable(creatable_providers), summaries)
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
    let authority = authority(&["openai"], Vec::new());
    let refusal = authority
        .validate_selection("openai", Some("frontier-imaginary"), "some-model")
        .expect_err("uncreatable provider must refuse");
    assert_eq!(
        refusal,
        SelectionRefusal::ProviderUnavailable {
            provider: "frontier-imaginary".to_owned()
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
        }
    );
    assert_eq!(refusal.kind(), "model_unknown");
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
    fetched.inventory_fetched_at_ms = Some(1);
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
    let authority = authority(&["openai", "anthropic-oauth"], Vec::new());
    assert_eq!(
        authority.validate_selection("openai", Some("anthropic-oauth"), "fable-5"),
        Ok(("anthropic-oauth".to_owned(), "fable-5".to_owned()))
    );
}

/// Selecting a row on the session's CURRENT provider never consults
/// creatability — the session already runs it (explicit == absent).
#[test]
fn current_provider_rows_skip_creatability() {
    let authority = ModelSelectionAuthority::new(None, Vec::new());
    assert_eq!(
        authority.validate_selection("fake", Some("fake"), "fake-model-2"),
        Ok(("fake".to_owned(), "fake-model-2".to_owned()))
    );
    assert_eq!(
        authority.validate_selection("fake", None, "fake-model-2"),
        Ok(("fake".to_owned(), "fake-model-2".to_owned()))
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
        let authority = ModelSelectionAuthority::new(creatable(&["openai"]), vec![custom]);
        assert_eq!(
            authority.validate_selection("openai", Some("my-endpoint"), "local-model"),
            Ok(("my-endpoint".to_owned(), "local-model".to_owned()))
        );
    }
}

/// An empty model is a malformed selector, not a lookup.
#[test]
fn empty_model_is_an_invalid_selector() {
    let authority = authority(&["openai"], Vec::new());
    assert!(matches!(
        authority.validate_selection("openai", None, "  "),
        Err(SelectionRefusal::InvalidSelector { .. })
    ));
}

// ───────────────────────────── child spawn selector ─────────────────────────

/// LAW (child_inherits_the_parents_current_pair_by_default): no selector →
/// the parent's CURRENT pair verbatim. The runtime half — inheritance after
/// a mid-session pair switch — is pinned in `pair_switch_runtime_tests.rs`.
#[test]
fn absent_selector_inherits_the_parents_current_pair() {
    let authority = authority(&["openai"], Vec::new());
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
    let SelectionRefusal::ModelNotResolvable { model, candidates } = &refusal else {
        panic!("expected ModelNotResolvable, got {refusal:?}");
    };
    assert_eq!(model, "shared-model");
    assert_eq!(candidates, &["kimi".to_owned(), "other".to_owned()]);
    assert!(refusal.message().contains("kimi"));
    assert!(refusal.message().contains("other"));
}

/// LAW (unavailable_is_typed): nobody serves the bare model → typed refusal
/// with EMPTY candidates and retry-with-explicit-pair guidance.
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
        }
    );
    assert!(refusal.message().contains("explicit"));
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
    let authority = authority(&["openai"], Vec::new());
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
    let authority = authority(&["bedrock", "vertex", "anthropic"], Vec::new());
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
