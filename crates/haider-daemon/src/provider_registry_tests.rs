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
        extensions: None,
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
    let summary = registry
        .summaries(&|_| false)
        .into_iter()
        .next()
        .expect("summary");
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
    let summary = registry
        .summaries(&|_| false)
        .into_iter()
        .next()
        .expect("summary");
    assert_eq!(summary.availability, ProviderAvailabilityWire::Unavailable);
    assert!(!summary.enabled);
    assert!(summary.models.is_empty());
    assert_eq!(summary.default_model, None);
}

/// MUTATION CHECK: route Gemini through the compatible OpenAI family or
/// bearer-auth metadata. Expected RUNTIME failure: the exact native family,
/// fixed Google origin, or API-key requirement below differs.
#[test]
fn gemini_is_a_builtin_generate_content_api_key_provider() {
    let source = model_source([(
        GEMINI_PROVIDER_NAME,
        vec![discovered_with_context(
            "gemini-2.5-flash",
            true,
            None,
            Some(1_048_576),
        )],
    )]);
    let registry = ProviderRegistry::new(
        MemoryProviderStore::default(),
        initial_provider_profiles(
            &std::collections::BTreeSet::from([GEMINI_PROVIDER_NAME.to_owned()]),
            "unused",
        ),
        source,
    )
    .expect("Gemini registry");
    let summary = registry
        .summary(GEMINI_PROVIDER_NAME, &|_| false)
        .expect("Gemini summary");
    assert_eq!(
        summary.api_family,
        ProviderApiFamilyWire::GeminiGenerateContent
    );
    assert_eq!(summary.endpoint.as_deref(), Some(GEMINI_API_BASE_URL));
    assert_eq!(summary.auth_methods, vec![AuthMethod::ApiKey]);
    assert_eq!(summary.models, vec!["gemini-2.5-flash"]);
    assert_eq!(summary.availability, ProviderAvailabilityWire::Available);
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

    let summary = registry
        .summaries(&|_| false)
        .into_iter()
        .next()
        .expect("summary");
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

    let summary = registry
        .summaries(&|_| false)
        .into_iter()
        .next()
        .expect("summary");
    assert_eq!(summary.models, vec!["frontier-a", "frontier-b"]);
    assert_eq!(
        summary.model_details,
        vec![
            ModelDetailWire {
                name: "frontier-a".to_owned(),
                context_window: Some(100_000),
                supported_efforts: Vec::new(),
                default_effort: None,
                supported_speeds: Vec::new(),
                supports_thinking_type: None,
            },
            ModelDetailWire {
                name: "frontier-b".to_owned(),
                context_window: Some(200_000),
                supported_efforts: Vec::new(),
                default_effort: None,
                supported_speeds: Vec::new(),
                supports_thinking_type: None,
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

    let summary = registry
        .summaries(&|_| false)
        .into_iter()
        .next()
        .expect("summary");
    assert!(summary.models.is_empty());
    assert_eq!(summary.default_model, None);
    assert_eq!(summary.availability, ProviderAvailabilityWire::Unavailable);
    assert_eq!(
        summary.availability_reason.as_deref(),
        Some("provider model inventory is unavailable")
    );
}

/// MUTATION CHECK: register Kimi as a generic/custom provider, API-key
/// provider, or Responses dialect. Expected RUNTIME failure: the release-owned
/// registry projection below changes its fixed endpoint or auth/API family.
#[test]
fn kimi_oauth_is_a_builtin_chat_completions_subscription_provider() {
    let source = model_source([(
        KIMI_OAUTH_PROVIDER_NAME,
        vec![discovered_with_context(
            "kimi-coding-a",
            true,
            None,
            Some(262_144),
        )],
    )]);
    let registry = ProviderRegistry::new(
        MemoryProviderStore::default(),
        initial_provider_profiles(
            &std::collections::BTreeSet::from([KIMI_OAUTH_PROVIDER_NAME.to_owned()]),
            "unused",
        ),
        source,
    )
    .expect("Kimi provider registry");
    let summary = registry
        .summaries(&|_| false)
        .into_iter()
        .next()
        .expect("Kimi summary");
    assert_eq!(summary.provider, KIMI_OAUTH_PROVIDER_NAME);
    assert_eq!(
        summary.api_family,
        ProviderApiFamilyWire::OpenAiChatCompletions
    );
    assert_eq!(summary.endpoint.as_deref(), Some(KIMI_OAUTH_BASE_URL));
    assert_eq!(summary.auth_methods, vec![AuthMethod::OAuth]);
    assert_eq!(summary.availability, ProviderAvailabilityWire::Available);
    assert_eq!(summary.models, vec!["kimi-coding-a"]);
    assert_eq!(summary.model_details[0].context_window, Some(262_144));
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

/// Only custom profiles are removable, and successful removal clears the
/// in-memory discovered inventory along with the persisted projection.
///
/// MUTATION CHECK: remove the provenance guard or the model-source deletion
/// from `remove_custom`. Expected RUNTIME failure: a release-owned profile is
/// removed or the custom provider's cached model remains readable.
#[test]
fn provider_registry_removes_only_custom_profiles_and_clears_models() {
    let source = model_source([
        ("custom", vec![discovered("custom-model", true, None)]),
        ("openai", vec![discovered("builtin-model", true, None)]),
    ]);
    let mut registry = ProviderRegistry::new(
        MemoryProviderStore::default(),
        initial_provider_profiles(
            &std::collections::BTreeSet::from([OPENAI_PROVIDER_NAME.to_owned()]),
            "unused",
        ),
        source.clone(),
    )
    .expect("provider registry");
    registry
        .configure(ProviderConfigureInput {
            provider: "custom".to_owned(),
            api_family: Some(ProviderApiFamilyWire::OpenAiChatCompletions),
            origin: Some("https://custom.example.invalid".to_owned()),
            auth_requirement: Some(ProviderAuthRequirementWire::ApiKey),
            enabled: true,
            models: vec!["custom-model".to_owned()],
            default_model: Some("custom-model".to_owned()),
        })
        .expect("create custom profile");

    let builtin = registry
        .remove_custom(OPENAI_PROVIDER_NAME)
        .expect_err("release-owned profile must be refused");
    assert_eq!(builtin.code, ErrorCode::InvalidArgument);
    registry
        .remove_custom("custom")
        .expect("remove custom profile");
    assert!(registry.get("custom").is_none());
    assert!(source.models("custom").is_none());
    assert!(registry.get(OPENAI_PROVIDER_NAME).is_some());
}

// ───────────────────────────── G4b enterprise seeds ─────────────────────────

fn seeded_registry(provider: &str) -> ProviderRegistry<MemoryProviderStore> {
    ProviderRegistry::new(
        MemoryProviderStore::default(),
        initial_provider_profiles(
            &std::collections::BTreeSet::from([provider.to_owned()]),
            "unused-literal",
        ),
        model_source([]),
    )
    .expect("registry")
}

/// LAW (LA-x — the seeded-list availability rule, decision 6): a bedrock or
/// vertex profile with its SEEDED model list lights Available exactly when
/// a credential exists AND an endpoint is configured — no discovery
/// requirement — and stays honestly Unavailable otherwise, with the reason
/// naming the missing piece. Both directions on the credential axis; the
/// vertex seed (no endpoint until its card runs) pins the endpoint axis.
///
/// MUTATION CHECK: drop the `credentialed` conjunct (or the `base_url`
/// conjunct) from `seeded_ready` in `provider_summary`. Expected RUNTIME
/// failure: the credential-less bedrock row (respectively the endpoint-less
/// vertex row) reports Available.
#[test]
fn la_seeded_list_providers_light_available_once_a_credential_exists() {
    let bedrock = seeded_registry("bedrock");
    let without_credential = bedrock
        .summary("bedrock", &|_| false)
        .expect("bedrock summary");
    assert_eq!(
        without_credential.availability,
        ProviderAvailabilityWire::Unavailable
    );
    assert_eq!(
        without_credential.availability_reason.as_deref(),
        Some("provider has no credential")
    );
    assert_eq!(
        without_credential.models,
        haider_provider::BEDROCK_SEED_MODELS
            .iter()
            .map(|slug| (*slug).to_owned())
            .collect::<Vec<_>>(),
        "the seeded list IS the inventory even before a credential"
    );

    let with_credential = bedrock
        .summary("bedrock", &|provider| provider == "bedrock")
        .expect("bedrock summary");
    assert_eq!(
        with_credential.availability,
        ProviderAvailabilityWire::Available,
        "a credential + the seeded default-region endpoint light bedrock"
    );
    assert_eq!(
        with_credential.endpoint.as_deref(),
        Some("https://bedrock-mantle.us-east-1.api.aws/anthropic")
    );
    assert_eq!(
        with_credential.default_model.as_deref(),
        Some("anthropic.claude-fable-5")
    );

    // Vertex seeds NO endpoint: a credential alone must not light it.
    let vertex = seeded_registry("vertex");
    let endpoint_less = vertex.summary("vertex", &|_| true).expect("vertex summary");
    assert_eq!(
        endpoint_less.availability,
        ProviderAvailabilityWire::Unavailable
    );
    assert_eq!(
        endpoint_less.availability_reason.as_deref(),
        Some("provider endpoint is not configured")
    );
    assert_eq!(
        endpoint_less.models,
        haider_provider::VERTEX_SEED_MODELS
            .iter()
            .map(|slug| (*slug).to_owned())
            .collect::<Vec<_>>()
    );
}

/// LAW (LE-x, wire-detail half): bedrock/vertex seeded model details carry
/// the normalized static effort ladders and defaults, but NEVER
/// `supported_speeds` — fast is Claude-API-only (decision 4) — while the
/// first-party anthropic detail keeps advertising fast (both directions).
///
/// MUTATION CHECK: add bedrock/vertex to the `supported_speeds` provider
/// match in `model_detail_wire`. Expected RUNTIME failure: the empty-speeds
/// assertions below.
#[test]
fn bedrock_and_vertex_model_details_get_effort_ladders_but_no_speeds() {
    let bedrock = seeded_registry("bedrock")
        .summary("bedrock", &|_| true)
        .expect("bedrock summary");
    let opus = bedrock
        .model_details
        .iter()
        .find(|detail| detail.name == "anthropic.claude-opus-5")
        .expect("seeded opus detail");
    assert_eq!(
        opus.supported_efforts,
        ["low", "medium", "high", "xhigh", "max"],
        "the normalized static ladder rides the seeded detail"
    );
    assert_eq!(opus.default_effort.as_deref(), Some("high"));
    assert!(
        opus.supported_speeds.is_empty(),
        "fast is Claude-API-only — no speeds on bedrock details"
    );
    assert!(
        bedrock
            .model_details
            .iter()
            .all(|detail| detail.supported_speeds.is_empty()),
        "no bedrock detail may advertise a speed"
    );
    assert_eq!(
        bedrock
            .model_details
            .iter()
            .find(|detail| detail.name == "anthropic.claude-haiku-4-5")
            .expect("seeded haiku detail")
            .context_window,
        None,
        "seeded rows never guess a context window"
    );

    let vertex = seeded_registry("vertex")
        .summary("vertex", &|_| true)
        .expect("vertex summary");
    let dated = vertex
        .model_details
        .iter()
        .find(|detail| detail.name == "claude-sonnet-4-5@20250929")
        .expect("dated vertex detail");
    assert!(
        dated.supported_efforts.is_empty(),
        "sonnet-4-5 documents no ladder — normalization never invents one"
    );
    assert!(
        vertex
            .model_details
            .iter()
            .all(|detail| detail.supported_speeds.is_empty())
    );

    // The FIRST-PARTY detail keeps fast (the other direction).
    let anthropic = ProviderRegistry::new(
        MemoryProviderStore::default(),
        initial_provider_profiles(
            &std::collections::BTreeSet::from([ANTHROPIC_PROVIDER_NAME.to_owned()]),
            "unused",
        ),
        model_source([(
            ANTHROPIC_PROVIDER_NAME,
            vec![discovered("claude-opus-5", true, None)],
        )]),
    )
    .expect("registry")
    .summary(ANTHROPIC_PROVIDER_NAME, &|_| false)
    .expect("anthropic summary");
    assert_eq!(
        anthropic.model_details[0].supported_speeds,
        ["fast"],
        "the claude api keeps advertising fast"
    );
}

/// LAW (LZ2 — the azure discovery-404 fallback): an Azure-origin CUSTOM
/// profile keeps its manually entered deployments as inventory when
/// discovery has nothing, and lights Available once its key exists; a
/// non-azure custom keeps the G4a rule (discovery is the only inventory
/// truth) — both directions.
///
/// MUTATION CHECK: drop the azure-origin arm from `seeded_inventory`.
/// Expected RUNTIME failure: the azure row below reports Unavailable with
/// an empty model list.
#[test]
fn lz2_azure_custom_keeps_manual_deployments_available_without_discovery() {
    let custom = |provider: &str, origin: &str| ProviderProfileV1 {
        provider_id: provider.to_owned(),
        display_name: provider.to_owned(),
        api_family: ProviderApiFamilyWire::OpenAiChatCompletions,
        base_url: Some(origin.to_owned()),
        enabled: true,
        auth_requirement: ProviderAuthRequirementWire::ApiKey,
        configured_models: vec!["my-gpt-deployment".to_owned()],
        default_model: Some("my-gpt-deployment".to_owned()),
        provenance: ProviderProvenance::Custom,
    };
    let store = MemoryProviderStore::default();
    store
        .save(&[
            custom("azure", "https://contoso.openai.azure.com/openai/v1"),
            custom("vllm", "https://gateway.example.com/v1"),
        ])
        .expect("seed profiles");
    let registry = ProviderRegistry::new(store, Vec::new(), model_source([])).expect("registry");

    let azure = registry
        .summary("azure", &|provider| provider == "azure")
        .expect("azure summary");
    assert_eq!(azure.availability, ProviderAvailabilityWire::Available);
    assert_eq!(azure.models, ["my-gpt-deployment"]);
    assert_eq!(azure.default_model.as_deref(), Some("my-gpt-deployment"));

    let keyless_azure = registry
        .summary("azure", &|_| false)
        .expect("azure summary");
    assert_eq!(
        keyless_azure.availability,
        ProviderAvailabilityWire::Unavailable,
        "a seeded inventory without a credential stays honest"
    );

    let generic = registry
        .summary("vllm", &|_| true)
        .expect("generic custom summary");
    assert_eq!(
        generic.availability,
        ProviderAvailabilityWire::Unavailable,
        "non-azure customs keep the G4a discovery-only inventory rule"
    );
    assert!(generic.models.is_empty());
    assert_eq!(
        generic.availability_reason.as_deref(),
        Some("provider model inventory is unavailable")
    );
}

/// LAW (G4b origin mutability): the enterprise builtins' origins move ONLY
/// through their shape validators — a region re-configure applies, an
/// off-template URL is refused with nothing stored, and custom profiles
/// keep the create-only identity law untouched (both directions; the
/// create-only half is additionally pinned by
/// `existing_custom_provider_identity_fields_are_create_only`).
///
/// MUTATION CHECK: skip the validator call in the enterprise origin-update
/// branch of `configured_profiles_with_inventory`. Expected RUNTIME
/// failure: the off-template configure below succeeds and stores the URL.
#[test]
fn enterprise_origin_reconfigure_is_shape_validated() {
    let mut registry = seeded_registry("bedrock");
    let models: Vec<String> = haider_provider::BEDROCK_SEED_MODELS
        .iter()
        .map(|slug| (*slug).to_owned())
        .collect();
    let reconfigure = |origin: &str| ProviderConfigureInput {
        provider: "bedrock".to_owned(),
        api_family: Some(ProviderApiFamilyWire::AnthropicMessages),
        origin: Some(origin.to_owned()),
        auth_requirement: Some(ProviderAuthRequirementWire::ApiKey),
        enabled: true,
        models: models.clone(),
        default_model: Some("anthropic.claude-fable-5".to_owned()),
    };

    let profile = registry
        .configure(reconfigure(
            "https://bedrock-mantle.eu-central-1.api.aws/anthropic",
        ))
        .expect("region re-configure applies");
    assert_eq!(
        profile.base_url.as_deref(),
        Some("https://bedrock-mantle.eu-central-1.api.aws/anthropic")
    );

    let refused = registry
        .configure(reconfigure("https://api.anthropic.com"))
        .expect_err("off-template origin must refuse");
    assert!(refused.message.contains("bedrock-mantle"));
    assert_eq!(
        registry
            .get("bedrock")
            .expect("profile persists")
            .base_url
            .as_deref(),
        Some("https://bedrock-mantle.eu-central-1.api.aws/anthropic"),
        "a refused origin stores NOTHING"
    );

    // Seeded default-model selection validates against the seeded list.
    registry
        .set_default_model("bedrock", "anthropic.claude-opus-5")
        .expect("seeded inventory serves default selection");
    assert!(
        registry
            .set_default_model("bedrock", "not-a-seeded-model")
            .is_err(),
        "an off-inventory default stays refused"
    );
}
