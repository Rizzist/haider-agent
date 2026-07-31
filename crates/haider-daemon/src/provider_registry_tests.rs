#![allow(clippy::expect_used)]

use super::*;

#[derive(Default)]
struct MemoryProviderStore {
    profiles: std::sync::Mutex<Vec<ProviderProfileV1>>,
}

impl ProviderRegistryStoreLike for MemoryProviderStore {
    fn load(&self) -> Result<Vec<ProviderProfileV1>, HaiderError> {
        Ok(self
            .profiles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone())
    }

    fn save(&self, profiles: &[ProviderProfileV1]) -> Result<(), HaiderError> {
        *self
            .profiles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = profiles.to_vec();
        Ok(())
    }
}

fn discovered(slug: &str, visible: bool, priority: Option<i64>) -> DiscoveredModel {
    discovered_with_context(slug, visible, priority, None)
}

fn discovered_with_context(
    slug: &str,
    visible: bool,
    priority: Option<i64>,
    context_window: Option<u64>,
) -> DiscoveredModel {
    DiscoveredModel {
        slug: slug.to_owned(),
        display_name: format!("Fixture {slug}"),
        context_window,
        description: Some("provider-owned fixture".to_owned()),
        default_effort: None,
        supported_efforts: Vec::new(),
        visible,
        priority,
    }
}

fn model_source(
    entries: impl IntoIterator<Item = (&'static str, Vec<DiscoveredModel>)>,
) -> Arc<CachedProviderModelSource> {
    let source = Arc::new(CachedProviderModelSource::default());
    for (provider, models) in entries {
        source.replace(provider.to_owned(), models);
    }
    source
}

/// MUTATION CHECK (review of record, W5c.2b): weaken the availability
/// derivation to `profile.enabled` alone (drop the
/// `api_family != Unknown` conjunct). Expected runtime failure: the enabled
/// Unknown-family profile below is reported `Available`.
///
/// This is the tolerant-decode case the factory pin above cannot reach: a
/// NEWER daemon writes an enabled profile with an api_family this build does
/// not know, `#[serde(other)]` decodes it as `Unknown`, and this daemon must
/// not advertise a provider it cannot construct an adapter for.
/// Verified by revert on 2026-07-30.
#[test]
fn enabled_profile_with_unknown_api_family_is_never_available() {
    let store = MemoryProviderStore::default();
    store
        .save(&[ProviderProfileV1 {
            provider_id: "from-the-future".to_owned(),
            display_name: "From The Future".to_owned(),
            api_family: ProviderApiFamilyWire::Unknown,
            base_url: Some("https://api.future.example".to_owned()),
            enabled: true,
            auth_requirement: ProviderAuthRequirementWire::Unknown,
            configured_models: vec!["future-model".to_owned()],
            default_model: Some("future-model".to_owned()),
            provenance: ProviderProvenance::Custom,
        }])
        .expect("seed store");
    let source = model_source([(
        "from-the-future",
        vec![discovered("frontier-future", true, None)],
    )]);
    let registry = ProviderRegistry::new(store, Vec::new(), source).expect("registry");
    let summary = registry.summaries().into_iter().next().expect("summary");
    assert_eq!(
        summary.availability,
        ProviderAvailabilityWire::Unavailable,
        "an adapter this build cannot construct must never render available"
    );
}

/// MUTATION CHECK: replace the unknown arm in `builtin_or_unknown` with the
/// Anthropic profile. Expected runtime failure: `future-provider` is reported
/// enabled/Available with the Anthropic default instead of unavailable.
#[test]
fn unknown_factory_provider_is_never_rendered_healthy() {
    let store = MemoryProviderStore::default();
    let registry = ProviderRegistry::new(
        store,
        initial_provider_profiles(
            &std::collections::BTreeSet::from(["future-provider".to_owned()]),
            "claude-test",
        ),
        model_source([]),
    )
    .expect("registry");
    let summary = registry.summaries().into_iter().next().expect("summary");
    assert_eq!(summary.availability, ProviderAvailabilityWire::Unavailable);
    assert!(!summary.enabled);
    assert!(summary.models.is_empty());
    assert_eq!(summary.default_model, None);
}

/// MUTATION CHECK: pass `configured_models` instead of `discovered_models` to
/// `validated_default` in `ProviderRegistry::configured_profiles`. Expected
/// runtime failure: the configured-but-undiscovered default is accepted
/// instead of returning `invalid_argument`.
/// Verified by revert on 2026-07-30.
#[test]
fn configured_default_must_come_from_the_discovered_inventory() {
    let store = MemoryProviderStore::default();
    let source = model_source([("custom", vec![discovered("frontier-a", true, None)])]);
    let mut registry =
        ProviderRegistry::new(store, Vec::new(), source).expect("empty provider registry");
    let error = registry
        .configure(ProviderConfigureInput {
            provider: "custom".to_owned(),
            api_family: Some(ProviderApiFamilyWire::OpenAiChatCompletions),
            origin: Some("https://models.example.com".to_owned()),
            auth_requirement: Some(ProviderAuthRequirementWire::ApiKey),
            enabled: true,
            models: vec!["configured-only".to_owned()],
            default_model: Some("configured-only".to_owned()),
        })
        .expect_err("undiscovered default must fail");
    assert_eq!(error.code, ErrorCode::InvalidArgument);
}

/// MUTATION CHECK: make `require_matching_identity` return `Ok(())`
/// unconditionally. Expected runtime failure: the existing custom provider
/// accepts the retargeting request instead of returning `invalid_argument`.
#[test]
fn existing_custom_provider_identity_fields_are_create_only() {
    let store = MemoryProviderStore::default();
    let source = model_source([("custom", vec![discovered("frontier-a", true, None)])]);
    let mut registry =
        ProviderRegistry::new(store, Vec::new(), source).expect("empty provider registry");
    registry
        .configure(ProviderConfigureInput {
            provider: "custom".to_owned(),
            api_family: Some(ProviderApiFamilyWire::OpenAiChatCompletions),
            origin: Some("https://models.example.com".to_owned()),
            auth_requirement: Some(ProviderAuthRequirementWire::ApiKey),
            enabled: true,
            models: vec!["frontier-a".to_owned()],
            default_model: Some("frontier-a".to_owned()),
        })
        .expect("create custom");
    let error = registry
        .configure(ProviderConfigureInput {
            provider: "custom".to_owned(),
            api_family: None,
            origin: Some("https://attacker.example".to_owned()),
            auth_requirement: None,
            enabled: true,
            models: vec!["frontier-a".to_owned()],
            default_model: Some("frontier-a".to_owned()),
        })
        .expect_err("identity mutation must fail");
    assert!(error.message.contains("create-only"));
}

/// Summary inventory comes only from the provider-owned cache and applies
/// the production picker visibility and priority rules.
///
/// MUTATION CHECK: replace `self.discovered_slugs(&profile.provider_id)` in
/// `ProviderRegistry::summary_profile` with
/// `profile.configured_models.clone()`. Expected runtime failure: the summary
/// exposes `literal-guess` instead of the visible provider-provenance entries.
/// Verified by revert on 2026-07-30.
#[test]
fn summaries_report_pickable_discovered_models_not_profile_literals() {
    let store = MemoryProviderStore::default();
    store
        .save(&[ProviderProfileV1 {
            provider_id: "custom".to_owned(),
            display_name: "Custom".to_owned(),
            api_family: ProviderApiFamilyWire::OpenAiChatCompletions,
            base_url: Some("https://models.example.com".to_owned()),
            enabled: true,
            auth_requirement: ProviderAuthRequirementWire::ApiKey,
            configured_models: vec!["literal-guess".to_owned()],
            default_model: Some("frontier-a".to_owned()),
            provenance: ProviderProvenance::Custom,
        }])
        .expect("seed store");
    let source = model_source([(
        "custom",
        vec![
            discovered("frontier-b", true, Some(20)),
            discovered("hidden-provider-entry", false, Some(0)),
            discovered("frontier-a", true, Some(10)),
        ],
    )]);
    let registry = ProviderRegistry::new(store, Vec::new(), source).expect("registry");

    let summary = registry.summaries().into_iter().next().expect("summary");
    assert_eq!(summary.models, vec!["frontier-a", "frontier-b"]);
    assert_eq!(summary.default_model.as_deref(), Some("frontier-a"));
    assert_eq!(summary.availability, ProviderAvailabilityWire::Available);
}

/// MUTATION CHECK: build `model_details` from raw or independently ordered
/// discovered models instead of the pickable inventory used for `models`.
/// Expected runtime failure: the hidden row appears or the exact ordered
/// name/window alignment below differs.
#[test]
fn summaries_align_model_details_with_pickable_models_and_windows() {
    let store = MemoryProviderStore::default();
    store
        .save(&[ProviderProfileV1 {
            provider_id: "custom".to_owned(),
            display_name: "Custom".to_owned(),
            api_family: ProviderApiFamilyWire::OpenAiChatCompletions,
            base_url: Some("https://models.example.com".to_owned()),
            enabled: true,
            auth_requirement: ProviderAuthRequirementWire::ApiKey,
            configured_models: Vec::new(),
            default_model: Some("frontier-a".to_owned()),
            provenance: ProviderProvenance::Custom,
        }])
        .expect("seed store");
    let source = model_source([(
        "custom",
        vec![
            discovered_with_context("frontier-b", true, Some(20), Some(200_000)),
            discovered_with_context("hidden-provider-entry", false, Some(0), Some(999_000)),
            discovered_with_context("frontier-a", true, Some(10), Some(100_000)),
        ],
    )]);
    let registry = ProviderRegistry::new(store, Vec::new(), source).expect("registry");

    let summary = registry.summaries().into_iter().next().expect("summary");
    assert_eq!(summary.models, vec!["frontier-a", "frontier-b"]);
    assert_eq!(
        summary.model_details,
        vec![
            ModelDetailWire {
                name: "frontier-a".to_owned(),
                context_window: Some(100_000),
            },
            ModelDetailWire {
                name: "frontier-b".to_owned(),
                context_window: Some(200_000),
            },
        ]
    );
    assert_eq!(
        summary.models,
        summary
            .model_details
            .iter()
            .map(|detail| detail.name.clone())
            .collect::<Vec<_>>()
    );
    assert!(
        !summary
            .models
            .iter()
            .any(|model| model == "hidden-provider-entry")
    );
    assert!(
        !summary
            .model_details
            .iter()
            .any(|detail| detail.name == "hidden-provider-entry")
    );
}

/// An installed adapter without cache provenance is not a model inventory.
///
/// MUTATION CHECK: delete the `!discovered_models.is_empty()` conjunct from
/// `provider_summary`'s `available` expression. Expected runtime failure: the
/// built-in row below reports `Available` despite having no cached models.
/// Verified by revert on 2026-07-30.
#[test]
fn builtin_without_cached_models_is_unknown_not_available_with_guesses() {
    let registry = ProviderRegistry::new(
        MemoryProviderStore::default(),
        initial_provider_profiles(
            &std::collections::BTreeSet::from([OPENAI_OAUTH_PROVIDER_NAME.to_owned()]),
            "unused-literal",
        ),
        model_source([]),
    )
    .expect("registry");

    let summary = registry.summaries().into_iter().next().expect("summary");
    assert!(summary.models.is_empty());
    assert_eq!(summary.default_model, None);
    assert_eq!(summary.availability, ProviderAvailabilityWire::Unavailable);
    assert_eq!(
        summary.availability_reason.as_deref(),
        Some("provider model inventory is unavailable")
    );
}

/// MUTATION CHECK (W5g-5 live fix): revert `configured_profiles` to
/// validating against the bare discovery cache (no stated-inventory
/// fallback). Expected runtime failure: the one-shot enabled create below
/// dies with "default model … is not in the configured model inventory" —
/// the exact chicken-and-egg the live probe hit (a NEW provider can never
/// have discovered models before it exists).
#[test]
fn a_new_provider_creates_one_shot_on_its_stated_inventory() {
    let store = MemoryProviderStore::default();
    // NOTHING discovered for this provider — the brand-new case.
    let source = model_source([]);
    let mut registry =
        ProviderRegistry::new(store, Vec::new(), source).expect("empty provider registry");
    let profile = registry
        .configure(ProviderConfigureInput {
            provider: "probe".to_owned(),
            api_family: Some(ProviderApiFamilyWire::OpenAiChatCompletions),
            origin: Some("http://127.0.0.1:18123/v1".to_owned()),
            auth_requirement: Some(ProviderAuthRequirementWire::ApiKey),
            enabled: true,
            models: vec!["probe-model".to_owned()],
            default_model: Some("probe-model".to_owned()),
        })
        .expect("a stated inventory carries the one-shot create");
    assert_eq!(profile.default_model.as_deref(), Some("probe-model"));
    assert!(profile.enabled);

    // The stated inventory is a BOOTSTRAP, not a bypass: a default outside
    // it still dies, discovery-authoritative law untouched.
    let error = registry
        .configure(ProviderConfigureInput {
            provider: "probe2".to_owned(),
            api_family: Some(ProviderApiFamilyWire::OpenAiChatCompletions),
            origin: Some("http://127.0.0.1:18123/v1".to_owned()),
            auth_requirement: Some(ProviderAuthRequirementWire::ApiKey),
            enabled: true,
            models: vec!["served".to_owned()],
            default_model: Some("unserved".to_owned()),
        })
        .expect_err("a default outside the stated inventory still fails");
    assert_eq!(error.code, ErrorCode::InvalidArgument);
}
