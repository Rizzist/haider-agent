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

#[test]
fn injected_factory_availability_is_registered_scoped_and_not_persisted() {
    let names = std::collections::BTreeSet::from([
        "fake".to_owned(),
        "openai".to_owned(),
        "anthropic".to_owned(),
    ]);
    let store: Arc<dyn ProviderRegistryStoreLike> = Arc::new(MemoryProviderStore::default());
    let mut registry = ProviderRegistry::new(
        store.clone(),
        initial_provider_profiles(&names, "unused"),
        Arc::new(CachedProviderModelSource::default()),
    )
    .expect("registry");
    for provider in &names {
        assert!(
            !registry
                .summary(provider, &|_| false)
                .expect("accounts-backed summary")
                .model_is_selectable("fixture-model")
        );
    }
    let registered = std::collections::BTreeSet::from(["fake".to_owned(), "openai".to_owned()]);
    registry = registry.with_injected_factory(&registered);
    for provider in &registered {
        let summary = registry
            .summary(provider, &|_| false)
            .expect("injected summary");
        assert!(summary.model_is_selectable("fixture-model"));
        assert!(summary.selection_reason(Some("fixture-model")).is_none());
    }
    assert!(
        !registry
            .summary("anthropic", &|_| false)
            .expect("unregistered accounts-backed provider")
            .model_is_selectable("fixture-model")
    );
    assert!(registry.summary("removed", &|_| true).is_none());

    // A known provider's explicit disablement and fresh negative membership
    // remain authoritative even when transport construction is injected.
    registry
        .profiles
        .iter_mut()
        .find(|profile| profile.provider_id == "openai")
        .expect("known profile")
        .enabled = false;
    let disabled = registry.summary("openai", &|_| false).expect("disabled");
    assert!(!disabled.model_is_selectable("fixture-model"));
    assert_eq!(
        disabled.selection_reason(None).as_deref(),
        Some("provider is disabled")
    );
    let mut fresh = registry.summary("fake", &|_| false).expect("fake");
    fresh.configured_models = vec!["missing".into()];
    fresh.inventory = haider_rpc::ModelInventoryWire::Fetched {
        fetched_at_ms: u64::MAX,
    };
    assert!(!fresh.model_is_selectable("missing"));

    let reloaded = ProviderRegistry::new(
        store,
        initial_provider_profiles(&names, "unused"),
        Arc::new(CachedProviderModelSource::default()),
    )
    .expect("reload without injected registration");
    for provider in &registered {
        assert!(
            !reloaded
                .summary(provider, &|_| false)
                .expect("production summary")
                .model_is_selectable("fixture-model")
        );
    }
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
        max_output_tokens: None,
        description: Some("provider-owned fixture".to_owned()),
        default_effort: None,
        supported_efforts: Vec::new(),
        visible,
        priority,
        use_responses_lite: None,
        extensions: None,
    }
}

fn model_source(
    entries: impl IntoIterator<Item = (&'static str, Vec<DiscoveredModel>)>,
) -> Arc<CachedProviderModelSource> {
    let source = Arc::new(CachedProviderModelSource::default());
    for (provider, models) in entries {
        source.replace(
            provider.to_owned(),
            ProviderInventory::Fetched {
                models,
                fetched_at_ms: u64::MAX,
            },
        );
    }
    source
}

#[test]
fn fresh_subscription_and_failed_catalog_keep_static_rows_selectable() {
    let source = model_source([]);
    let registry = ProviderRegistry::new(
        MemoryProviderStore::default(),
        initial_provider_profiles(
            &std::collections::BTreeSet::from(["anthropic-oauth".into()]),
            "unused",
        ),
        source.clone(),
    )
    .expect("registry");
    let fresh = registry
        .summary("anthropic-oauth", &|_| true)
        .expect("summary");
    assert_eq!(fresh.inventory, haider_rpc::ModelInventoryWire::Static);
    assert_eq!(
        fresh.availability,
        haider_rpc::ProviderAvailabilityWire::Available
    );
    assert_eq!(fresh.default_model.as_deref(), Some("claude-fable-5-1"));
    assert!(
        fresh
            .model_details
            .iter()
            .all(|row| row.source == Some(haider_rpc::ModelDetailSourceWire::Static))
    );
    assert!(
        fresh
            .model_details
            .iter()
            .find(|row| row.name == "claude-fable-5-1")
            .and_then(|row| row.context_window)
            .is_some()
    );

    source.unavailable(
        "anthropic-oauth",
        "provider does not serve a model list to this credential (403)".into(),
    );
    let fallback = registry
        .summary("anthropic-oauth", &|_| true)
        .expect("fallback");
    assert!(matches!(
        fallback.inventory,
        haider_rpc::ModelInventoryWire::Unavailable { .. }
    ));
    assert_eq!(
        fallback.availability,
        haider_rpc::ProviderAvailabilityWire::Available
    );
    assert_eq!(fallback.models, fresh.models);
    assert_eq!(
        registry
            .summary("anthropic-oauth", &|_| false)
            .expect("no credential")
            .availability,
        haider_rpc::ProviderAvailabilityWire::Unavailable
    );

    source.replace(
        "anthropic-oauth".into(),
        ProviderInventory::Fetched {
            models: vec![
                discovered_with_context("claude-fable-5-1", true, None, Some(123_456)),
                discovered("claude-new", true, None),
            ],
            fetched_at_ms: 1,
        },
    );
    let merged = registry
        .summary("anthropic-oauth", &|_| true)
        .expect("merged");
    let overridden = merged
        .model_details
        .iter()
        .find(|row| row.name == "claude-fable-5-1")
        .expect("overridden");
    assert!(
        merged.static_models.contains(&"claude-fable-5-1".into()),
        "release ownership is independent of fetched metadata precedence"
    );
    assert_eq!(overridden.context_window, Some(123_456));
    assert_eq!(
        overridden.source,
        Some(haider_rpc::ModelDetailSourceWire::Remote)
    );
    assert!(
        merged
            .model_details
            .iter()
            .any(|row| row.name == "claude-new"
                && row.source == Some(haider_rpc::ModelDetailSourceWire::Remote))
    );
    assert!(
        merged
            .model_details
            .iter()
            .any(|row| row.name == "claude-opus-5"
                && row.source == Some(haider_rpc::ModelDetailSourceWire::Static))
    );
}

#[test]
fn each_subscription_builtin_has_selectable_rows_before_discovery() {
    for provider in [
        "anthropic-oauth",
        "openai-oauth",
        "kimi-oauth",
        "grok-oauth",
        "haider-code",
    ] {
        let registry = ProviderRegistry::new(
            MemoryProviderStore::default(),
            initial_provider_profiles(
                &std::collections::BTreeSet::from([provider.into()]),
                "unused",
            ),
            model_source([]),
        )
        .expect("registry");
        let summary = registry.summary(provider, &|_| true).expect("summary");
        assert_eq!(
            summary.availability,
            ProviderAvailabilityWire::Available,
            "{provider}"
        );
        assert_eq!(
            summary.inventory,
            haider_rpc::ModelInventoryWire::Static,
            "{provider}"
        );
        assert!(!summary.models.is_empty(), "{provider}");
        assert!(
            summary
                .model_details
                .iter()
                .all(|row| row.source == Some(haider_rpc::ModelDetailSourceWire::Static)),
            "{provider}"
        );
    }
}

fn resilience_profiles() -> Vec<ProviderProfileV1> {
    initial_provider_profiles(
        &std::collections::BTreeSet::from([
            OPENAI_PROVIDER_NAME.to_owned(),
            ANTHROPIC_PROVIDER_NAME.to_owned(),
        ]),
        "unused",
    )
}

/// MUTATION CHECK: remove the byte comparison or compare only after replacing
/// the file. Expected runtime failure: an unchanged boot changes mtime/inode.
#[test]
fn unchanged_json_registry_boot_does_not_rewrite_the_file() {
    let dir = tempfile::tempdir().expect("provider registry profile");
    let store = JsonProviderRegistryStore::new(dir.path());
    store
        .save(&resilience_profiles())
        .expect("seed provider registry");
    let path = dir.path().join(PROVIDERS_FILE_NAME);
    let before = std::fs::metadata(&path).expect("registry metadata before boot");

    let _registry = ProviderRegistry::new(store, Vec::new(), model_source([]))
        .expect("unchanged provider registry boot");
    let after = std::fs::metadata(&path).expect("registry metadata after boot");

    assert_eq!(before.modified().ok(), after.modified().ok());
    assert_eq!(before.len(), after.len());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        assert_eq!(before.ino(), after.ino());
    }
}

/// MUTATION CHECK: skip every boot save instead of only a byte-identical one.
/// Expected runtime failure: the missing built-in profile is not published.
#[test]
fn changed_json_registry_boot_still_replaces_the_file() {
    let dir = tempfile::tempdir().expect("provider registry profile");
    let store = JsonProviderRegistryStore::new(dir.path());
    let profiles = resilience_profiles();
    store
        .save(&profiles[..1])
        .expect("seed incomplete provider registry");
    let path = dir.path().join(PROVIDERS_FILE_NAME);
    let before = std::fs::read(&path).expect("registry bytes before reconciliation");

    let _registry = ProviderRegistry::new(store, profiles, model_source([]))
        .expect("changed provider registry boot");
    let after = std::fs::read(&path).expect("registry bytes after reconciliation");

    assert_ne!(before, after);
    let document: ProviderRegistryDocumentV1 =
        serde_json::from_slice(&after).expect("reconciled registry document");
    assert_eq!(document.providers.len(), 2);
}

/// MUTATION CHECK: stop splitting the environment form on commas, stop
/// preferring it to the persisted chain, or accept unknown providers/empty
/// models. Expected runtime failure: the exact typed coordinates differ or a
/// malformed entry succeeds.
#[test]
fn fallback_chain_registry_and_environment_forms_are_validated() {
    let profiles = resilience_profiles();
    let persisted = vec!["anthropic/claude-opus-test".to_owned(), "openai".to_owned()];
    assert_eq!(
        resolve_fallback_chain(&persisted, None, &profiles).expect("registry chain"),
        vec![
            ProviderTargetV1 {
                provider: "anthropic".to_owned(),
                model: Some("claude-opus-test".to_owned()),
            },
            ProviderTargetV1 {
                provider: "openai".to_owned(),
                model: None,
            },
        ]
    );
    assert_eq!(
        resolve_fallback_chain(&persisted, Some(" openai/gpt-test , anthropic "), &profiles,)
            .expect("environment override"),
        vec![
            ProviderTargetV1 {
                provider: "openai".to_owned(),
                model: Some("gpt-test".to_owned()),
            },
            ProviderTargetV1 {
                provider: "anthropic".to_owned(),
                model: None,
            },
        ],
        "the environment replaces rather than appends to the durable chain"
    );
    assert!(
        resolve_fallback_chain(&persisted, Some("unknown/model"), &profiles)
            .expect_err("unknown provider must be refused")
            .message
            .contains("not registered")
    );
    assert!(
        resolve_fallback_chain(&persisted, Some("openai/"), &profiles)
            .expect_err("empty model must be refused")
            .message
            .contains("empty model")
    );
    assert!(
        resolve_fallback_chain(&persisted, Some("openai,,anthropic"), &profiles)
            .expect_err("empty entry must be refused")
            .message
            .contains("must not be empty")
    );
    assert!(
        resolve_fallback_chain(&persisted, Some(""), &profiles)
            .expect("an explicitly empty override disables fallback")
            .is_empty()
    );
}

/// The new object document is backward-compatible with legacy profile arrays,
/// and every ordinary profile save preserves its top-level fallback chain.
#[test]
fn json_registry_loads_and_preserves_top_level_fallback_chain() {
    let dir = tempfile::tempdir().expect("temporary provider registry");
    let path = dir.path().join(PROVIDERS_FILE_NAME);
    let profiles = resilience_profiles();
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "providers": profiles,
            "fallback_chain": ["openai/gpt-test", "anthropic"]
        }))
        .expect("registry JSON"),
    )
    .expect("write registry");
    let registry = ProviderRegistry::new(
        JsonProviderRegistryStore::new(dir.path()),
        Vec::new(),
        model_source([]),
    )
    .expect("object registry");
    assert_eq!(
        registry.fallback_chain(),
        [
            ProviderTargetV1 {
                provider: "openai".to_owned(),
                model: Some("gpt-test".to_owned()),
            },
            ProviderTargetV1 {
                provider: "anthropic".to_owned(),
                model: None,
            },
        ]
    );
    let rewritten: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("read rewritten registry"))
            .expect("rewritten JSON");
    assert_eq!(
        rewritten["fallback_chain"],
        serde_json::json!(["openai/gpt-test", "anthropic"])
    );

    let legacy_dir = tempfile::tempdir().expect("legacy registry");
    std::fs::write(
        legacy_dir.path().join(PROVIDERS_FILE_NAME),
        serde_json::to_vec(&resilience_profiles()).expect("legacy JSON"),
    )
    .expect("write legacy registry");
    let legacy = ProviderRegistry::new(
        JsonProviderRegistryStore::new(legacy_dir.path()),
        Vec::new(),
        model_source([]),
    )
    .expect("legacy array remains readable");
    assert!(legacy.fallback_chain().is_empty());
}

/// MUTATION CHECK: allow an explicit promotion provider to apply to another
/// current lane, or ignore the per-provider durable model. Expected runtime
/// failure: the cross-provider lookup succeeds or the local lookup is empty.
#[test]
fn compaction_promotion_is_validated_and_scoped_to_the_current_provider() {
    let mut profiles = resilience_profiles();
    profiles
        .iter_mut()
        .find(|profile| profile.provider_id == "openai")
        .expect("openai profile")
        .promotion_model = Some("gpt-large".to_owned());
    let registry = ProviderRegistry {
        store: MemoryProviderStore::default(),
        profiles: profiles.clone(),
        model_source: model_source([]),
        resilience: ProviderResilienceConfigV1 {
            fallback_chain: Vec::new(),
            promotion_models: std::collections::HashMap::from([(
                "openai".to_owned(),
                "gpt-large".to_owned(),
            )]),
            compaction_promotion_override: None,
        },
    };
    assert_eq!(
        registry.compaction_promotion("openai"),
        Some(ProviderTargetV1 {
            provider: "openai".to_owned(),
            model: Some("gpt-large".to_owned()),
        })
    );
    assert_eq!(registry.compaction_promotion("anthropic"), None);

    let explicit = ProviderRegistry {
        store: MemoryProviderStore::default(),
        profiles: profiles.clone(),
        model_source: model_source([]),
        resilience: ProviderResilienceConfigV1 {
            fallback_chain: Vec::new(),
            promotion_models: std::collections::HashMap::new(),
            compaction_promotion_override: Some(
                parse_promotion_override("openai/gpt-env-large", &profiles)
                    .expect("explicit override"),
            ),
        },
    };
    assert_eq!(explicit.compaction_promotion("anthropic"), None);
    assert_eq!(
        explicit.compaction_promotion("openai"),
        Some(ProviderTargetV1 {
            provider: "openai".to_owned(),
            model: Some("gpt-env-large".to_owned()),
        })
    );

    let relative = ProviderRegistry {
        store: MemoryProviderStore::default(),
        profiles: profiles.clone(),
        model_source: model_source([]),
        resilience: ProviderResilienceConfigV1 {
            fallback_chain: Vec::new(),
            promotion_models: std::collections::HashMap::new(),
            compaction_promotion_override: Some(
                parse_promotion_override("larger-current-model", &profiles)
                    .expect("relative override"),
            ),
        },
    };
    assert_eq!(
        relative.compaction_promotion("anthropic"),
        Some(ProviderTargetV1 {
            provider: "anthropic".to_owned(),
            model: Some("larger-current-model".to_owned()),
        })
    );
    assert!(parse_promotion_override("unknown/model", &profiles).is_err());
    assert!(parse_promotion_override("openai/", &profiles).is_err());
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
            response_open_timeout_ms: None,
            chunk_idle_timeout_ms: None,
            semantic_progress_timeout_ms: None,
            enabled: true,
            auth_requirement: ProviderAuthRequirementWire::Unknown,
            configured_models: vec!["future-model".to_owned()],
            default_model: Some("future-model".to_owned()),
            promotion_model: None,
            catalog_selection_key: None,
            provenance: ProviderProvenance::Custom,
            trust: haider_rpc::ProviderTrustWire::Full,
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
        .summary(GEMINI_PROVIDER_NAME, &|_| true)
        .expect("Gemini summary");
    assert_eq!(
        summary.api_family,
        ProviderApiFamilyWire::GeminiGenerateContent
    );
    assert_eq!(summary.endpoint.as_deref(), Some(GEMINI_API_BASE_URL));
    assert_eq!(summary.auth_methods, vec![AuthMethod::ApiKey]);
    assert_eq!(summary.models, vec!["gemini-2.5-flash"]);
    assert_eq!(summary.availability, ProviderAvailabilityWire::Available);
    assert!(
        !registry
            .summary(GEMINI_PROVIDER_NAME, &|_| false)
            .expect("missing credential")
            .model_is_selectable("gemini-2.5-flash")
    );
    assert_eq!(
        summary.inventory_authority,
        haider_rpc::ModelInventoryAuthorityWire::Authoritative
    );
}

/// v0.0.970: the supervised Antigravity agent registers as its OWN execution
/// kind — no HTTP endpoint, OAuth, and a deliberately EMPTY inventory so the
/// authenticated agent's own catalog is the only truth and the auto-discovery
/// trigger is not suppressed.
///
/// MUTATION CHECK: seed a model list on the `google-antigravity` arm (the
/// DeepSeek/Bedrock shape) instead of leaving it empty. Expected runtime
/// failure: `configured_models` is non-empty and the profile reports
/// `Available` before any account has ever authenticated.
#[test]
fn google_antigravity_registers_as_an_acp_agent_with_no_endpoint_and_no_seeded_models() {
    let profile = builtin_or_unknown(GOOGLE_ANTIGRAVITY_PROVIDER_NAME, "unused");
    assert_eq!(profile.api_family, ProviderApiFamilyWire::AcpAgent);
    assert_eq!(
        profile.base_url, None,
        "a supervised agent has no HTTP origin to publish"
    );
    assert_eq!(profile.auth_requirement, ProviderAuthRequirementWire::OAuth);
    assert!(profile.enabled);
    assert!(matches!(profile.provenance, ProviderProvenance::BuiltIn));
    assert!(
        profile.configured_models.is_empty(),
        "the agent's own catalog is the only inventory truth"
    );
    assert_eq!(profile.default_model, None);

    // Empty inventory means the summary is honestly UNAVAILABLE until the
    // first authenticated discovery fills it, which is what keeps the
    // auto-discovery trigger armed.
    let registry = ProviderRegistry::new(
        MemoryProviderStore::default(),
        initial_provider_profiles(
            &std::collections::BTreeSet::from([GOOGLE_ANTIGRAVITY_PROVIDER_NAME.to_owned()]),
            "unused",
        ),
        model_source([]),
    )
    .expect("Antigravity registry");
    let summary = registry
        .summary(GOOGLE_ANTIGRAVITY_PROVIDER_NAME, &|_| false)
        .expect("Antigravity summary");
    assert_eq!(summary.api_family, ProviderApiFamilyWire::AcpAgent);
    assert_eq!(summary.endpoint, None);
    assert_eq!(summary.auth_methods, vec![AuthMethod::OAuth]);
    assert!(summary.models.is_empty());
    assert_eq!(summary.default_model, None);
    assert_eq!(
        summary.availability,
        ProviderAvailabilityWire::Unavailable,
        "nothing is offered until the authenticated agent publishes a catalog"
    );

    assert_eq!(
        registry
            .summary(GOOGLE_ANTIGRAVITY_PROVIDER_NAME, &|_| true)
            .expect("healthy but unfetched")
            .availability,
        ProviderAvailabilityWire::Unavailable
    );

    // Once discovery publishes the agent's catalog, every reasoning variant
    // survives as its own row and the provider lights up.
    let discovered = model_source([(
        GOOGLE_ANTIGRAVITY_PROVIDER_NAME,
        vec![
            discovered("gemini-3.8-flash-high", true, None),
            discovered("gemini-3.8-flash-medium", true, None),
            discovered("gemini-pro-agent", true, None),
        ],
    )]);
    let registry = ProviderRegistry::new(
        MemoryProviderStore::default(),
        initial_provider_profiles(
            &std::collections::BTreeSet::from([GOOGLE_ANTIGRAVITY_PROVIDER_NAME.to_owned()]),
            "unused",
        ),
        discovered,
    )
    .expect("Antigravity registry with a catalog");
    let summary = registry
        .summary(GOOGLE_ANTIGRAVITY_PROVIDER_NAME, &|_| true)
        .expect("Antigravity summary with a catalog");
    assert_eq!(
        summary.models,
        vec![
            "gemini-3.8-flash-high",
            "gemini-3.8-flash-medium",
            "gemini-pro-agent"
        ]
    );
    assert_eq!(summary.availability, ProviderAvailabilityWire::Available);
    assert_eq!(
        summary.inventory_authority,
        haider_rpc::ModelInventoryAuthorityWire::Authoritative
    );
    assert!(
        summary
            .model_details
            .iter()
            .all(|detail| detail.supports_vision == Some(false)),
        "the ACP turn builder sends text blocks only, so the row refuses a paste \
         rather than silently dropping it"
    );
}

/// REGRESSION: admitting `google-antigravity` must not disturb the API-key
/// `gemini` provider. Its profile, family, endpoint, auth requirement and
/// request shape are all unchanged, and the two classes never alias.
///
/// MUTATION CHECK: point the `gemini` arm at the ACP family (or reuse the
/// `gemini` id for the supervised agent). Expected runtime failure: the family
/// or endpoint assertion below, and the distinct-id assertion.
#[test]
fn admitting_the_supervised_agent_leaves_the_api_key_gemini_provider_unchanged() {
    let gemini = builtin_or_unknown(GEMINI_PROVIDER_NAME, "unused");
    assert_eq!(gemini.provider_id, "gemini");
    assert_eq!(
        gemini.api_family,
        ProviderApiFamilyWire::GeminiGenerateContent
    );
    assert_eq!(gemini.base_url.as_deref(), Some(GEMINI_API_BASE_URL));
    assert_eq!(gemini.auth_requirement, ProviderAuthRequirementWire::ApiKey);
    assert!(gemini.enabled);
    assert!(gemini.configured_models.is_empty());
    assert_eq!(gemini.default_model, None);
    assert!(matches!(gemini.provenance, ProviderProvenance::BuiltIn));

    let antigravity = builtin_or_unknown(GOOGLE_ANTIGRAVITY_PROVIDER_NAME, "unused");
    assert_ne!(gemini.provider_id, antigravity.provider_id);
    assert_ne!(gemini.api_family, antigravity.api_family);
    assert_ne!(gemini.auth_requirement, antigravity.auth_requirement);
    assert_ne!(gemini.base_url, antigravity.base_url);

    // Its summary is unchanged too: same family, same endpoint, same
    // API-key auth, and a discovered catalog that is still its own.
    let registry = ProviderRegistry::new(
        MemoryProviderStore::default(),
        initial_provider_profiles(
            &std::collections::BTreeSet::from([
                GEMINI_PROVIDER_NAME.to_owned(),
                GOOGLE_ANTIGRAVITY_PROVIDER_NAME.to_owned(),
            ]),
            "unused",
        ),
        model_source([
            (
                GEMINI_PROVIDER_NAME,
                vec![discovered("gemini-2.5-flash", true, None)],
            ),
            (
                GOOGLE_ANTIGRAVITY_PROVIDER_NAME,
                vec![discovered("gemini-3.8-flash-high", true, None)],
            ),
        ]),
    )
    .expect("both Google lanes register side by side");
    let gemini_summary = registry
        .summary(GEMINI_PROVIDER_NAME, &|_| true)
        .expect("Gemini summary");
    assert_eq!(
        gemini_summary.api_family,
        ProviderApiFamilyWire::GeminiGenerateContent
    );
    assert_eq!(
        gemini_summary.endpoint.as_deref(),
        Some(GEMINI_API_BASE_URL)
    );
    assert_eq!(gemini_summary.auth_methods, vec![AuthMethod::ApiKey]);
    assert_eq!(gemini_summary.models, vec!["gemini-2.5-flash"]);
    assert_eq!(
        gemini_summary.availability,
        ProviderAvailabilityWire::Available
    );
    let antigravity_summary = registry
        .summary(GOOGLE_ANTIGRAVITY_PROVIDER_NAME, &|_| true)
        .expect("Antigravity summary");
    assert_eq!(antigravity_summary.models, vec!["gemini-3.8-flash-high"]);
    assert_ne!(gemini_summary.models, antigravity_summary.models);
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
            response_open_timeout_ms: None,
            chunk_idle_timeout_ms: None,
            semantic_progress_timeout_ms: None,
            trust: None,
        })
        .expect_err("undiscovered default must fail");
    assert_eq!(error.code, ErrorCode::InvalidArgument);
}

/// MUTATION CHECK: change `ProviderTrustWire::default()` to Lockdown.
/// Expected failure: a record written before the trust field existed silently
/// loses capabilities during upgrade.
#[test]
fn legacy_provider_record_without_trust_migrates_to_full() {
    let mut legacy_custom = resilience_profiles()
        .into_iter()
        .next()
        .expect("profile template");
    legacy_custom.provider_id = "existing-custom".to_owned();
    legacy_custom.provenance = ProviderProvenance::Custom;
    let mut value = serde_json::to_value(legacy_custom).expect("profile JSON");
    value
        .as_object_mut()
        .expect("profile object")
        .remove("trust");
    let profile: ProviderProfileV1 = serde_json::from_value(value).expect("legacy profile");
    assert_eq!(profile.provenance, ProviderProvenance::Custom);
    assert_eq!(profile.trust, ProviderTrustWire::Full);
}

/// MUTATION CHECK: default a new custom provider to Lockdown, or fail to save
/// the staged preview. Expected failure: the preview or unchanged registry assertion.
#[test]
fn new_custom_provider_defaults_full_and_trust_preview_is_unpublished() {
    let dir = tempfile::tempdir().expect("provider registry profile");
    let source = model_source([("research", vec![discovered("search-1", true, None)])]);
    let mut registry = ProviderRegistry::new(
        JsonProviderRegistryStore::new(dir.path()),
        Vec::new(),
        source.clone(),
    )
    .expect("empty provider registry");
    let profile = registry
        .configure(ProviderConfigureInput {
            provider: "research".to_owned(),
            api_family: Some(ProviderApiFamilyWire::OpenAiChatCompletions),
            origin: Some("https://research.example.invalid/v1".to_owned()),
            auth_requirement: Some(ProviderAuthRequirementWire::None),
            enabled: true,
            models: vec!["search-1".to_owned()],
            default_model: Some("search-1".to_owned()),
            response_open_timeout_ms: None,
            chunk_idle_timeout_ms: None,
            semantic_progress_timeout_ms: None,
            trust: None,
        })
        .expect("create custom provider");
    assert_eq!(profile.trust, ProviderTrustWire::Full);
    let preview = registry
        .preview_trust("research", ProviderTrustWire::Lockdown, &|_| false)
        .expect("preview trust");
    assert_eq!(preview.trust, ProviderTrustWire::Lockdown);
    assert_eq!(
        registry.get("research").map(|profile| profile.trust),
        Some(ProviderTrustWire::Full)
    );
    registry.publish_trust("research", preview.trust);
    assert_eq!(
        registry.get("research").map(|profile| profile.trust),
        Some(ProviderTrustWire::Lockdown)
    );
    // The caller's committed SQLite receipt, not an independent JSON write,
    // supplies the durable override when the actor restarts.
    let reopened = ProviderRegistry::new(
        JsonProviderRegistryStore::new(dir.path()),
        Vec::new(),
        source,
    )
    .expect("reopen provider registry");
    assert_eq!(
        reopened.get("research").map(|profile| profile.trust),
        Some(ProviderTrustWire::Full)
    );
}

/// `provider.configure` carries trust for atomic creation and crash replay,
/// but it must never become an unjournaled toggle for an existing provider.
/// An exact-value retry remains idempotent so a create receipt can reconcile.
///
/// MUTATION CHECK: restore the assignment to `existing.trust`. Expected
/// failure: the first reconfigure silently widens the provider to Full.
#[test]
fn existing_provider_trust_changes_require_the_typed_setter() {
    let store = MemoryProviderStore::default();
    let source = model_source([]);
    let mut registry =
        ProviderRegistry::new(store, Vec::new(), source).expect("empty provider registry");
    let create = ProviderConfigureInput {
        provider: "research".to_owned(),
        api_family: Some(ProviderApiFamilyWire::OpenAiChatCompletions),
        origin: Some("https://research.example/v1".to_owned()),
        auth_requirement: Some(ProviderAuthRequirementWire::ApiKey),
        enabled: true,
        models: vec!["research-model".to_owned()],
        default_model: Some("research-model".to_owned()),
        response_open_timeout_ms: None,
        chunk_idle_timeout_ms: None,
        semantic_progress_timeout_ms: None,
        trust: Some(ProviderTrustWire::Lockdown),
    };
    registry
        .configure(create.clone())
        .expect("new provider accepts explicit lockdown trust");

    let mut widen = create.clone();
    widen.api_family = None;
    widen.origin = None;
    widen.auth_requirement = None;
    widen.trust = Some(ProviderTrustWire::Full);
    let error = registry
        .configure(widen)
        .expect_err("configure cannot bypass provider.set_trust");
    assert_eq!(error.code, ErrorCode::InvalidArgument);
    assert!(error.message.contains("provider.set_trust"));
    assert_eq!(
        registry.get("research").map(|profile| profile.trust),
        Some(ProviderTrustWire::Lockdown)
    );

    let mut replay = create.clone();
    replay.api_family = None;
    replay.origin = None;
    replay.auth_requirement = None;
    let profile = registry
        .configure(replay)
        .expect("same-trust configure replay remains idempotent");
    assert_eq!(profile.trust, ProviderTrustWire::Lockdown);

    let mut unknown = create;
    unknown.provider = "future-trust".to_owned();
    unknown.origin = Some("https://future.example/v1".to_owned());
    unknown.trust = Some(ProviderTrustWire::Unknown);
    let error = registry
        .configure(unknown)
        .expect_err("unknown trust cannot enter a new durable record");
    assert_eq!(error.code, ErrorCode::InvalidArgument);
}

#[test]
fn all_built_in_provider_records_are_full_trust() {
    assert!(
        resilience_profiles()
            .iter()
            .all(|profile| profile.trust == ProviderTrustWire::Full)
    );
}

#[test]
fn provider_response_open_timeout_override_must_be_nonzero() {
    let mut registry =
        ProviderRegistry::new(MemoryProviderStore::default(), Vec::new(), model_source([]))
            .expect("empty provider registry");
    let error = registry
        .configure(ProviderConfigureInput {
            provider: "probe-zero-timeout".to_owned(),
            api_family: Some(ProviderApiFamilyWire::OpenAiChatCompletions),
            origin: Some("http://127.0.0.1:18124/v1".to_owned()),
            auth_requirement: Some(ProviderAuthRequirementWire::ApiKey),
            enabled: true,
            models: vec!["probe-model".to_owned()],
            default_model: Some("probe-model".to_owned()),
            response_open_timeout_ms: Some(0),
            chunk_idle_timeout_ms: None,
            semantic_progress_timeout_ms: None,
            trust: None,
        })
        .expect_err("a zero response-open budget must fail typed validation");
    assert_eq!(error.code, ErrorCode::InvalidArgument);
    assert!(error.message.contains("greater than zero"));
}

/// MUTATION CHECK: make `require_matching_identity` return `Ok(())`, or remove
/// the custom-auth update. Expected failure: family mutation succeeds or the
/// in-place keyless transition is refused.
#[test]
fn existing_custom_provider_keeps_api_family_and_switches_auth_in_place() {
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
            response_open_timeout_ms: None,
            chunk_idle_timeout_ms: None,
            semantic_progress_timeout_ms: None,
            trust: None,
        })
        .expect("create custom");
    let error = registry
        .configure(ProviderConfigureInput {
            provider: "custom".to_owned(),
            api_family: Some(ProviderApiFamilyWire::AnthropicMessages),
            origin: None,
            auth_requirement: None,
            enabled: true,
            models: vec!["frontier-a".to_owned()],
            default_model: Some("frontier-a".to_owned()),
            response_open_timeout_ms: None,
            chunk_idle_timeout_ms: None,
            semantic_progress_timeout_ms: None,
            trust: None,
        })
        .expect_err("API-family mutation must fail");
    assert!(error.message.contains("cannot change its API family"));

    let no_auth = registry
        .configure(ProviderConfigureInput {
            provider: "custom".to_owned(),
            api_family: None,
            origin: None,
            auth_requirement: Some(ProviderAuthRequirementWire::None),
            enabled: true,
            models: vec!["frontier-a".to_owned()],
            default_model: Some("frontier-a".to_owned()),
            response_open_timeout_ms: None,
            chunk_idle_timeout_ms: None,
            semantic_progress_timeout_ms: None,
            trust: None,
        })
        .expect("custom auth requirement is mutable in place");
    assert_eq!(no_auth.auth_requirement, ProviderAuthRequirementWire::None);
    let keyed = registry
        .configure(ProviderConfigureInput {
            provider: "custom".to_owned(),
            api_family: None,
            origin: None,
            auth_requirement: Some(ProviderAuthRequirementWire::ApiKey),
            enabled: true,
            models: vec!["frontier-a".to_owned()],
            default_model: Some("frontier-a".to_owned()),
            response_open_timeout_ms: None,
            chunk_idle_timeout_ms: None,
            semantic_progress_timeout_ms: None,
            trust: None,
        })
        .expect("custom provider can return to keyed mode");
    assert_eq!(keyed.auth_requirement, ProviderAuthRequirementWire::ApiKey);
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
            response_open_timeout_ms: None,
            chunk_idle_timeout_ms: None,
            semantic_progress_timeout_ms: None,
            enabled: true,
            auth_requirement: ProviderAuthRequirementWire::ApiKey,
            configured_models: vec!["literal-guess".to_owned()],
            default_model: Some("frontier-a".to_owned()),
            promotion_model: None,
            catalog_selection_key: None,
            provenance: ProviderProvenance::Custom,
            trust: haider_rpc::ProviderTrustWire::Full,
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
        .summaries(&|_| true)
        .into_iter()
        .next()
        .expect("summary");
    assert_eq!(summary.models, vec!["frontier-a", "frontier-b"]);
    let summary_json = serde_json::to_value(&summary).expect("summary JSON");
    assert_eq!(
        summary_json["model_details"][0]["display_name"], "Fixture frontier-a",
        "provider catalog display names must survive the registry projection"
    );
    assert_eq!(summary.default_model.as_deref(), Some("frontier-a"));
    assert_eq!(summary.availability, ProviderAvailabilityWire::Available);
    let unavailable = registry
        .summary("custom", &|_| false)
        .expect("credential negative");
    assert_eq!(unavailable.models, summary.models);
    assert!(!unavailable.model_is_selectable("frontier-a"));
}

/// A custom OpenAI-compatible model id is endpoint vocabulary, while the
/// discovered catalog is only advisory picker inventory.
///
/// MUTATION CHECK: restore the discovered-membership filter in
/// `provider_summary` or classify custom provenance as authoritative. Expected
/// runtime failure: the configured default becomes `None` or the custom
/// authority assertion differs for both the mismatched and empty catalogs.
#[test]
fn custom_summary_preserves_configured_default_across_mismatched_and_empty_catalog() {
    let profile = ProviderProfileV1 {
        provider_id: "bench-proxy".to_owned(),
        display_name: "Bench Proxy".to_owned(),
        api_family: ProviderApiFamilyWire::OpenAiChatCompletions,
        base_url: Some("https://bench.example.invalid/v1".to_owned()),
        response_open_timeout_ms: None,
        chunk_idle_timeout_ms: None,
        semantic_progress_timeout_ms: None,
        enabled: true,
        auth_requirement: ProviderAuthRequirementWire::None,
        configured_models: vec!["deepseek-v4-flash".to_owned()],
        default_model: Some("deepseek-v4-flash".to_owned()),
        promotion_model: None,
        catalog_selection_key: None,
        provenance: ProviderProvenance::Custom,
        trust: haider_rpc::ProviderTrustWire::Full,
    };
    let store = MemoryProviderStore::default();
    store
        .save(std::slice::from_ref(&profile))
        .expect("seed store");
    let source = model_source([(
        "bench-proxy",
        vec![discovered("canonical-other", true, None)],
    )]);
    let registry = ProviderRegistry::new(store, Vec::new(), source.clone()).expect("registry");

    let mismatched = registry
        .summary("bench-proxy", &|_| true)
        .expect("mismatched summary");
    assert_eq!(mismatched.models, ["canonical-other"]);
    assert_eq!(
        mismatched.inventory_authority,
        haider_rpc::ModelInventoryAuthorityWire::Advisory
    );
    assert_eq!(
        mismatched.default_model.as_deref(),
        Some("deepseek-v4-flash")
    );

    source.remove("bench-proxy");
    let empty = registry
        .summary("bench-proxy", &|_| true)
        .expect("empty summary");
    assert_eq!(empty.models, ["deepseek-v4-flash"]);
    assert_eq!(
        empty.inventory_authority,
        haider_rpc::ModelInventoryAuthorityWire::Advisory
    );
    assert_eq!(empty.default_model.as_deref(), Some("deepseek-v4-flash"));
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
            response_open_timeout_ms: None,
            chunk_idle_timeout_ms: None,
            semantic_progress_timeout_ms: None,
            enabled: true,
            auth_requirement: ProviderAuthRequirementWire::ApiKey,
            configured_models: Vec::new(),
            default_model: Some("frontier-a".to_owned()),
            promotion_model: None,
            catalog_selection_key: None,
            provenance: ProviderProvenance::Custom,
            trust: haider_rpc::ProviderTrustWire::Full,
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
    // 970: `supports_vision` stays ABSENT here. This profile is a CUSTOM
    // OpenAI-compatible provider, whose model slugs are user-controlled, so
    // the catalog declares nothing rather than turning the adapter's "we
    // cannot infer this" into a refusal the client would enforce.
    assert_eq!(
        summary.model_details,
        vec![
            ModelDetailWire {
                source: Some(haider_rpc::ModelDetailSourceWire::Remote),
                name: "frontier-a".to_owned(),
                display_name: Some("Fixture frontier-a".to_owned()),
                context_window: Some(100_000),
                max_output_tokens: Some(haider_provider::UNKNOWN_OUTPUT_LIMIT),
                supported_efforts: Vec::new(),
                default_effort: None,
                supported_speeds: Vec::new(),
                supports_thinking_type: None,
                supports_vision: None,
            },
            ModelDetailWire {
                source: Some(haider_rpc::ModelDetailSourceWire::Remote),
                name: "frontier-b".to_owned(),
                display_name: Some("Fixture frontier-b".to_owned()),
                context_window: Some(200_000),
                max_output_tokens: Some(haider_provider::UNKNOWN_OUTPUT_LIMIT),
                supported_efforts: Vec::new(),
                default_effort: None,
                supported_speeds: Vec::new(),
                supports_thinking_type: None,
                supports_vision: None,
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

/// A static subscription catalog may be visible before login, but it cannot
/// make the provider available without a healthy credential.
#[test]
fn builtin_static_models_stay_unavailable_without_credential() {
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
    assert!(summary.models.contains(&"gpt-6-sol".to_owned()));
    assert_eq!(summary.inventory, haider_rpc::ModelInventoryWire::Static);
    assert_eq!(summary.availability, ProviderAvailabilityWire::Unavailable);
    assert_eq!(
        summary.availability_reason.as_deref(),
        Some("provider has no credential")
    );
}

#[test]
fn openai_subscription_summary_excludes_incompatible_and_legacy_cached_rows() {
    let mut ready = discovered("lite-ready", true, Some(3));
    ready.use_responses_lite = Some(true);
    let mut standard = discovered("standard-only", true, Some(1));
    standard.use_responses_lite = Some(false);
    let source = model_source([(
        OPENAI_OAUTH_PROVIDER_NAME,
        vec![standard, discovered("legacy-row", true, Some(2)), ready],
    )]);
    let registry = ProviderRegistry::new(
        MemoryProviderStore::default(),
        initial_provider_profiles(
            &std::collections::BTreeSet::from([OPENAI_OAUTH_PROVIDER_NAME.to_owned()]),
            "legacy-row",
        ),
        source,
    )
    .expect("registry");
    let summary = registry
        .summary(OPENAI_OAUTH_PROVIDER_NAME, &|_| true)
        .expect("OpenAI summary");
    assert!(summary.models.contains(&"lite-ready".to_owned()));
    assert!(!summary.models.contains(&"standard-only".to_owned()));
    assert!(!summary.models.contains(&"legacy-row".to_owned()));
    assert!(!summary.models.contains(&"gpt-5.5".to_owned()));
    assert_eq!(summary.model_details.len(), 7, "six static plus one remote");
    assert_eq!(summary.default_model.as_deref(), Some("gpt-6-astra"));
}

#[test]
fn openai_static_lite_capability_survives_undeclared_remote_metadata() {
    let mut explicit_no = discovered("gpt-6-sol", true, Some(0));
    explicit_no.use_responses_lite = Some(false);
    let undeclared = discovered("gpt-6-luna", true, Some(1));
    let registry = ProviderRegistry::new(
        MemoryProviderStore::default(),
        initial_provider_profiles(
            &std::collections::BTreeSet::from([OPENAI_OAUTH_PROVIDER_NAME.to_owned()]),
            "unused",
        ),
        model_source([(OPENAI_OAUTH_PROVIDER_NAME, vec![explicit_no, undeclared])]),
    )
    .expect("registry");
    let summary = registry
        .summary(OPENAI_OAUTH_PROVIDER_NAME, &|_| true)
        .expect("summary");
    assert!(
        !summary.models.contains(&"gpt-6-sol".to_owned()),
        "explicit false wins"
    );
    assert!(
        summary.models.contains(&"gpt-6-luna".to_owned()),
        "static known capability fills missing flag"
    );
    assert!(!summary.models.contains(&"gpt-5.5".to_owned()));
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
        .summaries(&|_| true)
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
    assert!(summary.models.contains(&"kimi-coding-a".to_owned()));
    assert!(summary.models.contains(&"kimi-for-coding".to_owned()));
    assert_eq!(
        summary
            .model_details
            .iter()
            .find(|row| row.name == "kimi-coding-a")
            .expect("remote row")
            .context_window,
        Some(262_144)
    );
}

/// WH1 — `deepseek` is release-owned Chat Completions at the fixed vendor
/// base with API-key auth. Its documented aliases are only a fallback, so
/// they carry no guessed context window or effort ladder and stay
/// unavailable until a credential exists.
#[test]
fn wh1_deepseek_registry_is_builtin_chat_completions_api_key() {
    let registry = ProviderRegistry::new(
        MemoryProviderStore::default(),
        initial_provider_profiles(
            &std::collections::BTreeSet::from([DEEPSEEK_PROVIDER_NAME.to_owned()]),
            "unused",
        ),
        model_source([]),
    )
    .expect("DeepSeek provider registry");
    let profile = registry
        .get(DEEPSEEK_PROVIDER_NAME)
        .expect("DeepSeek profile");
    assert_eq!(profile.provenance, ProviderProvenance::BuiltIn);
    assert!(profile.configured_models.is_empty());

    let signed_out = registry
        .summary(DEEPSEEK_PROVIDER_NAME, &|_| false)
        .expect("signed-out DeepSeek summary");
    assert_eq!(signed_out.provider, DEEPSEEK_PROVIDER_NAME);
    assert_eq!(
        signed_out.api_family,
        ProviderApiFamilyWire::OpenAiChatCompletions
    );
    assert_eq!(signed_out.endpoint.as_deref(), Some(DEEPSEEK_BASE_URL));
    assert_eq!(signed_out.auth_methods, vec![AuthMethod::ApiKey]);
    assert!(signed_out.models.is_empty());
    assert_eq!(
        signed_out.availability,
        ProviderAvailabilityWire::Unavailable
    );
    assert!(signed_out.model_details.iter().all(|detail| {
        detail.context_window.is_none()
            && detail.supported_efforts.is_empty()
            && detail.supported_speeds.is_empty()
    }));

    let credentialed = registry
        .summary(DEEPSEEK_PROVIDER_NAME, &|provider| {
            provider == DEEPSEEK_PROVIDER_NAME
        })
        .expect("credentialed DeepSeek summary");
    assert_eq!(
        credentialed.availability,
        ProviderAvailabilityWire::Unavailable
    );
}

/// The public catalog has a conservative release snapshot for offline startup.
/// Authentication still belongs to inference, not catalog discovery.
#[test]
fn haider_code_registry_is_builtin_chat_completions_api_key() {
    let registry = ProviderRegistry::new(
        MemoryProviderStore::default(),
        initial_provider_profiles(
            &std::collections::BTreeSet::from([HAIDER_CODE_PROVIDER_NAME.to_owned()]),
            "unused",
        ),
        model_source([]),
    )
    .expect("Haider Code provider registry");
    let profile = registry
        .get(HAIDER_CODE_PROVIDER_NAME)
        .expect("Haider Code profile");
    assert_eq!(profile.provenance, ProviderProvenance::BuiltIn);
    assert!(profile.configured_models.is_empty());

    let summary = registry
        .summary(HAIDER_CODE_PROVIDER_NAME, &|provider| {
            provider == HAIDER_CODE_PROVIDER_NAME
        })
        .expect("Haider Code summary");
    assert_eq!(summary.provider, HAIDER_CODE_PROVIDER_NAME);
    assert_eq!(
        summary.api_family,
        ProviderApiFamilyWire::OpenAiChatCompletions
    );
    assert_eq!(summary.endpoint.as_deref(), Some(HAIDER_CODE_BASE_URL));
    assert_eq!(summary.auth_methods, vec![AuthMethod::ApiKey]);
    assert!(summary.models.contains(&"deepseek-v4-flash".to_owned()));
    assert_eq!(summary.inventory, haider_rpc::ModelInventoryWire::Static);
    assert_eq!(summary.default_model.as_deref(), Some("deepseek-v4-flash"));
    assert!(
        registry
            .validate_default_model(HAIDER_CODE_PROVIDER_NAME, "Go")
            .is_err()
    );
    assert_eq!(summary.availability, ProviderAvailabilityWire::Available);
    assert!(profile.default_model.is_none());
}

/// MUTATION CHECK: xAI and Grok OAuth must remain two distinct release-owned
/// profiles: API-key traffic uses api.x.ai while subscription traffic uses
/// the dedicated CLI proxy and OAuth authentication.
#[test]
fn xai_and_grok_oauth_registry_profiles_pin_lane_boundaries() {
    let names = std::collections::BTreeSet::from([
        XAI_PROVIDER_NAME.to_owned(),
        GROK_OAUTH_PROVIDER_NAME.to_owned(),
    ]);
    let registry = ProviderRegistry::new(
        MemoryProviderStore::default(),
        initial_provider_profiles(&names, "unused"),
        model_source([]),
    )
    .expect("xAI provider registry");

    let xai = registry
        .summary(XAI_PROVIDER_NAME, &|provider| provider == XAI_PROVIDER_NAME)
        .expect("xAI API summary");
    assert_eq!(xai.api_family, ProviderApiFamilyWire::OpenAiChatCompletions);
    assert_eq!(xai.endpoint.as_deref(), Some(XAI_BASE_URL));
    assert_eq!(xai.auth_methods, vec![AuthMethod::ApiKey]);
    assert!(xai.models.is_empty());
    assert!(xai.model_details.is_empty());
    assert_eq!(xai.inventory, haider_rpc::ModelInventoryWire::NeverFetched);
    assert!(xai.default_model.is_none());
    assert_eq!(xai.availability, ProviderAvailabilityWire::Unavailable);

    let grok = registry
        .summary(GROK_OAUTH_PROVIDER_NAME, &|provider| {
            provider == GROK_OAUTH_PROVIDER_NAME
        })
        .expect("Grok OAuth summary");
    assert_eq!(
        grok.api_family,
        ProviderApiFamilyWire::OpenAiChatCompletions
    );
    assert_eq!(grok.endpoint.as_deref(), Some(GROK_OAUTH_BASE_URL));
    assert_eq!(grok.auth_methods, vec![AuthMethod::OAuth]);
    assert!(grok.models.contains(&"grok-4.6".to_owned()));
    assert!(
        grok.model_details
            .iter()
            .all(|detail| detail.source == Some(haider_rpc::ModelDetailSourceWire::Static))
    );
    assert_eq!(grok.inventory, haider_rpc::ModelInventoryWire::Static);
    assert_eq!(grok.availability, ProviderAvailabilityWire::Available);
}

/// 973: replacing an account clears the previous account's fetched rows,
/// leaving the inventory unknown while the profile keeps its default. A
/// configure that re-submits the UNCHANGED default must not be refused; a NEW
/// default outside the stated inventory still is.
/// MUTATION CHECK: drop the unchanged-default carve-out in
/// `configured_profiles`; the re-submission below fails with "default model
/// `discovered-default` is not in the configured model inventory".
#[test]
fn unknown_inventory_keeps_an_unchanged_default_but_refuses_a_new_one() {
    let store = MemoryProviderStore::default();
    let source = model_source([]);
    let mut registry =
        ProviderRegistry::new(store, Vec::new(), source).expect("empty provider registry");
    let input = |models: Vec<String>, default_model: &str| ProviderConfigureInput {
        provider: "custom-reset".to_owned(),
        api_family: Some(ProviderApiFamilyWire::OpenAiChatCompletions),
        origin: Some("http://127.0.0.1:18124/v1".to_owned()),
        auth_requirement: Some(ProviderAuthRequirementWire::ApiKey),
        enabled: true,
        models,
        default_model: Some(default_model.to_owned()),
        response_open_timeout_ms: None,
        chunk_idle_timeout_ms: None,
        semantic_progress_timeout_ms: None,
        trust: None,
    };
    registry
        .configure(input(
            vec!["discovered-default".to_owned()],
            "discovered-default",
        ))
        .expect("create on the stated inventory");
    registry.replace_discovered_models(
        "custom-reset".to_owned(),
        vec![discovered("discovered-default", true, None)],
        1,
    );
    // The account change clears the fetched rows; the default survives.
    registry.clear_discovered_models("custom-reset");
    let kept = registry
        .configure(input(Vec::new(), "discovered-default"))
        .expect("an unchanged default is not re-validated against an unknown inventory");
    assert_eq!(kept.default_model.as_deref(), Some("discovered-default"));
    let error = registry
        .configure(input(Vec::new(), "invented-default"))
        .expect_err("a new default outside the stated inventory is refused");
    assert!(
        error
            .message
            .contains("is not in the configured model inventory")
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
            response_open_timeout_ms: Some(75_000),
            chunk_idle_timeout_ms: Some(95_000),
            semantic_progress_timeout_ms: Some(305_000),
            trust: None,
        })
        .expect("a stated inventory carries the one-shot create");
    assert_eq!(profile.default_model.as_deref(), Some("probe-model"));
    assert_eq!(profile.response_open_timeout_ms, Some(75_000));
    assert_eq!(profile.chunk_idle_timeout_ms, Some(95_000));
    assert_eq!(profile.semantic_progress_timeout_ms, Some(305_000));
    assert_eq!(
        registry
            .summary("probe", &|_| true)
            .expect("configured provider summary")
            .response_open_timeout_ms,
        Some(75_000),
        "the durable typed profile override reaches adapter resolution"
    );
    let summary = registry
        .summary("probe", &|_| true)
        .expect("configured provider summary");
    assert_eq!(summary.chunk_idle_timeout_ms, Some(95_000));
    assert_eq!(summary.semantic_progress_timeout_ms, Some(305_000));
    assert!(profile.enabled);
    let updated = registry
        .configure(ProviderConfigureInput {
            provider: "probe".to_owned(),
            api_family: None,
            origin: None,
            auth_requirement: None,
            enabled: true,
            models: vec!["probe-model".to_owned()],
            default_model: Some("probe-model".to_owned()),
            response_open_timeout_ms: None,
            chunk_idle_timeout_ms: None,
            semantic_progress_timeout_ms: None,
            trust: None,
        })
        .expect("an omitted timeout preserves the durable profile override");
    assert_eq!(updated.response_open_timeout_ms, Some(75_000));
    assert_eq!(updated.chunk_idle_timeout_ms, Some(95_000));
    assert_eq!(updated.semantic_progress_timeout_ms, Some(305_000));

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
            response_open_timeout_ms: None,
            chunk_idle_timeout_ms: None,
            semantic_progress_timeout_ms: None,
            trust: None,
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
            response_open_timeout_ms: None,
            chunk_idle_timeout_ms: None,
            semantic_progress_timeout_ms: None,
            trust: None,
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

/// MUTATION CHECK (release-owned origin): treat every existing profile like
/// `ProviderProvenance::Custom` in `require_matching_identity`. Expected
/// RUNTIME failure: the fixed OpenAI origin mutation succeeds or advances to
/// later validation instead of returning the origin-mutability refusal.
#[test]
fn ordinary_release_owned_provider_origin_remains_immutable() {
    let mut registry = seeded_registry(OPENAI_PROVIDER_NAME);
    let before = registry
        .get(OPENAI_PROVIDER_NAME)
        .expect("openai builtin")
        .base_url
        .clone();
    let error = registry
        .configure(ProviderConfigureInput {
            provider: OPENAI_PROVIDER_NAME.to_owned(),
            api_family: None,
            origin: Some("https://attacker.example.invalid/v1".to_owned()),
            auth_requirement: None,
            enabled: true,
            models: Vec::new(),
            default_model: None,
            response_open_timeout_ms: None,
            chunk_idle_timeout_ms: None,
            semantic_progress_timeout_ms: None,
            trust: None,
        })
        .expect_err("fixed release-owned origin must refuse");
    assert!(error.message.contains("origin is mutable only"));
    assert_eq!(
        registry
            .get(OPENAI_PROVIDER_NAME)
            .expect("openai remains")
            .base_url,
        before
    );
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
        Some(200_000),
        "seeded rows use the pinned Claude-family context fallback"
    );
    assert_eq!(
        bedrock
            .model_details
            .iter()
            .find(|detail| detail.name == "anthropic.claude-haiku-4-5")
            .expect("seeded haiku detail")
            .max_output_tokens,
        Some(64_000)
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
    assert_eq!(anthropic.model_details[0].context_window, Some(1_000_000));
    assert_eq!(
        anthropic.model_details[0].max_output_tokens,
        Some(128_000),
        "an incomplete subscription catalog receives the pinned row limit"
    );
}

/// LAW (LZ2 — the azure discovery-404 fallback): an Azure-origin CUSTOM
/// profile keeps its manually entered deployments visible when discovery has
/// nothing. Configured rows do not claim that remote discovery succeeded;
/// both Azure and other custom origins remain Unavailable until a fetch.
///
/// MUTATION CHECK: drop the configured-row fallback from `seeded_inventory`.
/// Expected runtime failure: the Azure and generic rows below are empty.
#[test]
fn lz2_azure_custom_keeps_manual_deployments_visible_without_discovery() {
    let custom = |provider: &str, origin: &str| ProviderProfileV1 {
        provider_id: provider.to_owned(),
        display_name: provider.to_owned(),
        api_family: ProviderApiFamilyWire::OpenAiChatCompletions,
        base_url: Some(origin.to_owned()),
        response_open_timeout_ms: None,
        chunk_idle_timeout_ms: None,
        semantic_progress_timeout_ms: None,
        enabled: true,
        auth_requirement: ProviderAuthRequirementWire::ApiKey,
        configured_models: vec!["my-gpt-deployment".to_owned()],
        default_model: Some("my-gpt-deployment".to_owned()),
        promotion_model: None,
        catalog_selection_key: None,
        provenance: ProviderProvenance::Custom,
        trust: haider_rpc::ProviderTrustWire::Full,
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
    assert_eq!(azure.availability, ProviderAvailabilityWire::Unavailable);
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
        "configured rows do not establish remote availability"
    );
    assert_eq!(generic.models, ["my-gpt-deployment"]);
    assert_eq!(
        generic.availability_reason.as_deref(),
        Some("model list not fetched — fetch models before selecting")
    );
}

/// LAW (G4b origin mutability): the enterprise builtins' origins move ONLY
/// through their shape validators — a region re-configure applies and an
/// off-template URL is refused with nothing stored. Custom-profile origin
/// repoints are governed separately by the account actor's shared endpoint
/// validator.
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
        response_open_timeout_ms: None,
        chunk_idle_timeout_ms: None,
        semantic_progress_timeout_ms: None,
        trust: None,
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

#[tokio::test]
async fn production_endpoint_validator_keeps_transport_failure_distinct_from_bad_origin() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve closed port");
    let address = listener.local_addr().expect("closed port address");
    drop(listener);
    let error = ProductionProviderEndpointValidator
        .validate(&format!("http://{address}"))
        .await
        .expect_err("closed loopback endpoint is unreachable");
    assert_eq!(error.code, ErrorCode::ProviderError);
    assert!(error.retryable);

    let invalid = ProductionProviderEndpointValidator
        .validate("http://203.0.113.7")
        .await
        .expect_err("public plain HTTP is invalid");
    assert_eq!(invalid.code, ErrorCode::InvalidArgument);
    assert!(!invalid.retryable);
}

#[test]
fn public_inventory_transitions_preserve_provenance_and_validate_defaults() {
    let source = model_source([]);
    let mut registry = ProviderRegistry::new(
        MemoryProviderStore::default(),
        initial_provider_profiles(
            &std::collections::BTreeSet::from([HAIDER_CODE_PROVIDER_NAME.to_owned()]),
            "unused",
        ),
        source.clone(),
    )
    .expect("registry");
    let current = || {
        registry
            .summary(HAIDER_CODE_PROVIDER_NAME, &|_| true)
            .expect("summary")
    };
    assert_eq!(current().inventory, haider_rpc::ModelInventoryWire::Static);
    source.unavailable(HAIDER_CODE_PROVIDER_NAME, "catalog transport failed".into());
    let unavailable = current();
    assert!(matches!(
        unavailable.inventory,
        haider_rpc::ModelInventoryWire::Unavailable { .. }
    ));
    assert_eq!(
        unavailable.availability,
        haider_rpc::ProviderAvailabilityWire::Available
    );
    assert!(
        unavailable
            .availability_reason
            .as_deref()
            .expect("visible transport uncertainty")
            .contains("catalog transport failed")
    );
    assert_eq!(
        unavailable.default_model.as_deref(),
        Some("deepseek-v4-flash")
    );
    source.replace(
        HAIDER_CODE_PROVIDER_NAME.to_owned(),
        ProviderInventory::Fetched {
            models: Vec::new(),
            fetched_at_ms: 1,
        },
    );
    source.unavailable(
        HAIDER_CODE_PROVIDER_NAME,
        "empty cache is not prior inventory".into(),
    );
    assert!(matches!(
        current().inventory,
        haider_rpc::ModelInventoryWire::Unavailable { .. }
    ));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis() as u64;
    source.replace(
        HAIDER_CODE_PROVIDER_NAME.to_owned(),
        ProviderInventory::Fetched {
            models: vec![discovered("deepseek-v4-flash", true, None)],
            fetched_at_ms: now,
        },
    );
    assert!(matches!(
        current().inventory,
        haider_rpc::ModelInventoryWire::Fetched { .. }
    ));
    assert_eq!(
        current().default_model.as_deref(),
        Some("deepseek-v4-flash")
    );
    source.unavailable(HAIDER_CODE_PROVIDER_NAME, "temporary outage".into());
    let stale = current();
    assert!(matches!(
        stale.inventory,
        haider_rpc::ModelInventoryWire::Stale { .. }
    ));
    assert_eq!(stale.models, ["deepseek-v4-flash"]);
    assert_eq!(
        stale.availability,
        haider_rpc::ProviderAvailabilityWire::Available
    );
    assert!(
        stale
            .availability_reason
            .as_deref()
            .expect("visible uncertainty")
            .contains("temporary outage")
    );
    source.replace(
        HAIDER_CODE_PROVIDER_NAME.to_owned(),
        ProviderInventory::Fetched {
            models: Vec::new(),
            fetched_at_ms: now,
        },
    );
    assert!(matches!(
        current().inventory,
        haider_rpc::ModelInventoryWire::Fetched { .. }
    ));
    assert!(!current().inventory.needs_refresh());
    assert_eq!(
        current().default_model.as_deref(),
        Some("deepseek-v4-flash")
    );
    assert!(
        registry
            .set_default_model(HAIDER_CODE_PROVIDER_NAME, "Go")
            .is_err()
    );
}

#[test]
fn upgrade_removes_persisted_public_seed_without_creating_aliases() {
    let mut old = builtin_or_unknown(HAIDER_CODE_PROVIDER_NAME, "unused");
    old.configured_models = vec!["Go".into(), "Go Max".into()];
    old.default_model = Some("Go".into());
    let store = MemoryProviderStore {
        profiles: std::sync::Mutex::new(vec![old]),
    };
    let registry = ProviderRegistry::new(store, Vec::new(), model_source([])).expect("upgrade");
    let profile = registry.get(HAIDER_CODE_PROVIDER_NAME).expect("profile");
    assert!(profile.configured_models.is_empty());
    assert!(profile.default_model.is_none());
    assert_eq!(
        registry
            .summary(HAIDER_CODE_PROVIDER_NAME, &|_| true)
            .expect("summary")
            .inventory,
        haider_rpc::ModelInventoryWire::Static
    );
}

#[test]
fn configured_deployments_are_selectable_only_after_success_or_unsupported_discovery() {
    let mut registry = seeded_registry("openai");
    registry
        .configure(ProviderConfigureInput {
            provider: "azure-test".into(),
            api_family: Some(ProviderApiFamilyWire::OpenAiChatCompletions),
            origin: Some("https://fixture.openai.azure.com/openai/v1".into()),
            auth_requirement: Some(ProviderAuthRequirementWire::ApiKey),
            enabled: true,
            models: vec!["my-deployment".into()],
            default_model: Some("my-deployment".into()),
            response_open_timeout_ms: None,
            chunk_idle_timeout_ms: None,
            semantic_progress_timeout_ms: None,
            trust: None,
        })
        .expect("configured deployment");
    assert!(
        registry
            .validate_default_model("azure-test", "my-deployment")
            .is_err()
    );
    for (reason, unsupported) in [
        ("server returned 404 for /models", true),
        ("server returned 405 for /models", true),
        ("model listing is explicitly not supported", true),
        ("server returned 503 for /models", false),
        ("model catalog discovery timed out", false),
        ("model catalog authentication failed", false),
    ] {
        if unsupported {
            registry.models_unsupported("azure-test", reason.into());
        } else if reason.contains("authentication") {
            registry.models_rejected("azure-test", reason.into());
        } else {
            registry.models_unavailable("azure-test", reason.into());
        }
        let selectable = !reason.contains("authentication");
        let summary = registry.summary("azure-test", &|_| true).expect("summary");
        assert_eq!(summary.models, ["my-deployment"]);
        let authority =
            crate::model_select::ModelSelectionAuthority::new(None, vec![summary.clone()]);
        assert_eq!(
            authority
                .validate_selection_with_status("azure-test", None, "my-deployment")
                .is_ok(),
            selectable,
            "session and picker selection must agree: {reason}"
        );
        assert_eq!(
            summary.availability == ProviderAvailabilityWire::Available,
            selectable,
            "{reason}"
        );
        assert_eq!(
            registry
                .validate_default_model("azure-test", "my-deployment")
                .is_ok(),
            selectable,
            "selection agrees with summary"
        );
        assert!(
            summary
                .availability_reason
                .as_deref()
                .expect("specific reason")
                .contains(reason)
        );
        if unsupported {
            assert!(
                summary
                    .availability_reason
                    .as_deref()
                    .expect("label")
                    .contains("configured (unverified)")
            );
            assert_eq!(
                registry
                    .summary("azure-test", &|_| false)
                    .expect("keyless")
                    .availability,
                ProviderAvailabilityWire::Unavailable
            );
        }
    }
    registry.replace_discovered_models(
        "azure-test".into(),
        vec![offline_model("azure-test", "my-deployment")],
        u64::MAX,
    );
    let fetched = registry.summary("azure-test", &|_| true).expect("fetched");
    assert_eq!(fetched.availability, ProviderAvailabilityWire::Available);
    assert!(
        registry
            .validate_default_model("azure-test", "my-deployment")
            .is_ok()
    );
    registry.models_unavailable("azure-test", "server returned 503 for /models".into());
    let failed = registry
        .summary("azure-test", &|_| true)
        .expect("failed after success");
    assert_eq!(failed.availability, ProviderAvailabilityWire::Available);
    assert!(
        failed
            .availability_reason
            .as_deref()
            .expect("reason")
            .contains("503")
    );
    assert!(
        registry
            .validate_default_model("azure-test", "my-deployment")
            .is_ok()
    );
    assert!(
        crate::model_select::ModelSelectionAuthority::new(None, vec![failed])
            .validate_selection_with_status("azure-test", None, "my-deployment")
            .is_ok()
    );
}

#[test]
fn fetched_default_is_scoped_durably_and_configured_deployment_is_a_negative_control() {
    let dir = tempfile::tempdir().expect("profile");
    let store = JsonProviderRegistryStore::new(dir.path());
    let mut registry =
        ProviderRegistry::new(store, Vec::new(), model_source([])).expect("registry");
    registry
        .configure(ProviderConfigureInput {
            provider: "work".into(),
            api_family: Some(ProviderApiFamilyWire::OpenAiChatCompletions),
            origin: Some("https://fixture.example.test/v1".into()),
            auth_requirement: Some(ProviderAuthRequirementWire::ApiKey),
            enabled: true,
            models: vec!["configured-deployment".into()],
            default_model: Some("configured-deployment".into()),
            response_open_timeout_ms: None,
            chunk_idle_timeout_ms: None,
            semantic_progress_timeout_ms: None,
            trust: None,
        })
        .expect("provider");
    registry
        .reconcile_catalog_scope("work", Some("incarnation-A".into()))
        .expect("A scope");
    registry.replace_discovered_models(
        "work".into(),
        vec![offline_model("work", "a-private")],
        u64::MAX,
    );
    registry
        .set_default_model("work", "a-private")
        .expect("fetched selection");
    registry
        .configure(ProviderConfigureInput {
            provider: "work".into(),
            api_family: None,
            origin: None,
            auth_requirement: None,
            enabled: true,
            models: Vec::new(),
            default_model: None,
            response_open_timeout_ms: None,
            chunk_idle_timeout_ms: None,
            semantic_progress_timeout_ms: None,
            trust: None,
        })
        .expect("configuration-only update");
    let profile = registry.get("work").expect("same-incarnation profile");
    assert_eq!(profile.default_model.as_deref(), Some("a-private"));
    assert_eq!(profile.configured_models, ["configured-deployment"]);
    drop(registry);
    let mut registry = ProviderRegistry::new(
        JsonProviderRegistryStore::new(dir.path()),
        Vec::new(),
        model_source([]),
    )
    .expect("restart");
    registry
        .reconcile_catalog_scope("work", Some("incarnation-A".into()))
        .expect("same scope");
    assert_eq!(
        registry
            .get("work")
            .expect("profile")
            .default_model
            .as_deref(),
        Some("a-private"),
        "same incarnation retains its selected default"
    );
    registry
        .reconcile_catalog_scope("work", Some("incarnation-B".into()))
        .expect("replacement");
    assert!(
        registry
            .get("work")
            .expect("profile")
            .default_model
            .is_none()
    );
    registry.replace_discovered_models(
        "work".into(),
        vec![offline_model("work", "b-current")],
        u64::MAX,
    );
    assert_eq!(
        registry
            .summary("work", &|_| true)
            .expect("B summary")
            .default_model
            .as_deref(),
        Some("b-current")
    );
    registry.models_unsupported("work", "not supported".into());
    registry
        .set_default_model("work", "configured-deployment")
        .expect("explicit config");
    registry
        .reconcile_catalog_scope("work", Some("incarnation-C".into()))
        .expect("new account");
    assert_eq!(
        registry
            .get("work")
            .expect("profile")
            .default_model
            .as_deref(),
        Some("configured-deployment")
    );
    drop(registry);
    let registry = ProviderRegistry::new(
        JsonProviderRegistryStore::new(dir.path()),
        Vec::new(),
        model_source([]),
    )
    .expect("restart after replacement");
    assert_ne!(
        registry
            .get("work")
            .expect("profile")
            .default_model
            .as_deref(),
        Some("a-private")
    );
}

#[test]
fn retired_catalog_failover_models_are_removed_from_the_durable_document() {
    let dir = tempfile::tempdir().expect("profile");
    let path = dir.path().join("providers.json");
    let mut profile = builtin_or_unknown("work", "unused");
    profile.provenance = ProviderProvenance::Custom;
    profile.api_family = ProviderApiFamilyWire::OpenAiChatCompletions;
    profile.auth_requirement = ProviderAuthRequirementWire::ApiKey;
    profile.enabled = true;
    profile.base_url = Some("https://fixture.example.test".into());
    profile.catalog_selection_key = Some("A".into());
    profile.configured_models = vec!["explicit-deployment".into()];
    profile.default_model = Some("a-private".into());
    profile.promotion_model = Some("a-private-large".into());
    std::fs::write(
        &path,
        serde_json::to_vec(&ProviderRegistryDocumentV1 {
            providers: vec![profile],
            fallback_chain: vec!["work/a-private".into()],
            ..ProviderRegistryDocumentV1::default()
        })
        .expect("document"),
    )
    .expect("seed");
    let mut registry = ProviderRegistry::new(
        JsonProviderRegistryStore::new(dir.path()),
        vec![],
        model_source([]),
    )
    .expect("registry");
    registry
        .reconcile_catalog_scope("work", Some("B".into()))
        .expect("retire A");
    let document: ProviderRegistryDocumentV1 =
        serde_json::from_slice(&std::fs::read(path).expect("read")).expect("document");
    assert_eq!(document.fallback_chain, ["work"]);
    assert!(document.providers[0].default_model.is_none());
    assert!(document.providers[0].promotion_model.is_none());
    assert_eq!(registry.fallback_chain()[0].model, None);
    assert_eq!(
        document.providers[0].configured_models,
        ["explicit-deployment"]
    );
}

#[test]
fn discovery_recovery_does_not_promote_fetched_slugs_into_configured_deployments() {
    for retired in [false, true] {
        let mut registry = seeded_registry("openai");
        let input = ProviderConfigureInput {
            provider: "recovery-work".into(),
            api_family: Some(ProviderApiFamilyWire::OpenAiChatCompletions),
            origin: Some("https://synthetic.example.test/v1".into()),
            auth_requirement: Some(ProviderAuthRequirementWire::ApiKey),
            enabled: true,
            models: vec!["a-private".into()],
            default_model: Some("a-private".into()),
            response_open_timeout_ms: None,
            chunk_idle_timeout_ms: None,
            semantic_progress_timeout_ms: None,
            trust: None,
        };
        registry
            .configure(input.clone())
            .expect("partially saved legacy discovery");
        if retired {
            registry
                .reconcile_catalog_scope("recovery-work", Some("B".into()))
                .expect("account changed before recovery");
        }
        let mut original = input;
        original.models.clear();
        if retired {
            original.default_model = None;
        }
        let fetched = if retired {
            Vec::new()
        } else {
            vec!["a-private".into()]
        };
        registry
            .reconcile_discovered_configure(original, &fetched, retired)
            .expect("recovery");
        assert!(
            registry
                .get("recovery-work")
                .expect("profile")
                .configured_models
                .is_empty()
        );
        if retired {
            assert!(
                registry
                    .get("recovery-work")
                    .expect("profile")
                    .default_model
                    .is_none()
            );
        }
        registry
            .reconcile_catalog_scope("recovery-work", Some("B".into()))
            .expect("replacement");
        assert!(
            registry
                .get("recovery-work")
                .expect("profile")
                .default_model
                .is_none()
        );
    }
}

#[test]
fn summary_rejects_foreign_default_without_startup_reconciliation() {
    let mut profile = removable_provider_profile_for_catalog_test();
    profile.default_model = Some("a-private-beta".into());
    let source = model_source([]);
    source.replace(
        "work".into(),
        ProviderInventory::Fetched {
            models: vec![discovered("b-current", true, None)],
            fetched_at_ms: u64::MAX,
        },
    );
    let registry = ProviderRegistry::new(MemoryProviderStore::default(), vec![profile], source)
        .expect("registry");
    assert_eq!(
        registry
            .get("work")
            .expect("profile")
            .default_model
            .as_deref(),
        Some("a-private-beta")
    );
    assert_eq!(
        registry
            .summary("work", &|_| true)
            .expect("summary")
            .default_model
            .as_deref(),
        Some("b-current")
    );
}

fn removable_provider_profile_for_catalog_test() -> ProviderProfileV1 {
    let mut profile = builtin_or_unknown("work", "unused");
    profile.provenance = ProviderProvenance::Custom;
    profile.api_family = ProviderApiFamilyWire::OpenAiChatCompletions;
    profile.base_url = Some("http://127.0.0.1:19373/v1".into());
    profile.auth_requirement = ProviderAuthRequirementWire::None;
    profile.configured_models.clear();
    profile.enabled = true;
    profile
}

#[test]
fn configured_availability_retains_stale_success_and_rejects_negative_evidence() {
    for origin in [
        "https://synthetic.openai.azure.com/openai/v1",
        "http://127.0.0.1:19373/v1",
    ] {
        let mut profile = removable_provider_profile_for_catalog_test();
        profile.base_url = Some(origin.into());
        profile.configured_models = vec!["deployment".into()];
        profile.default_model = Some("deployment".into());
        let source = model_source([]);
        let registry = ProviderRegistry::new(
            MemoryProviderStore::default(),
            vec![profile],
            source.clone(),
        )
        .expect("registry");
        let assert_policy = |expected| {
            let summary = registry.summary("work", &|_| true).expect("summary");
            assert_eq!(
                summary.model_is_selectable("deployment"),
                expected,
                "{origin}: {summary:?}"
            );
            assert_eq!(
                registry
                    .validate_default_model("work", "deployment")
                    .is_ok(),
                expected
            );
            assert_eq!(
                crate::model_select::ModelSelectionAuthority::new(None, vec![summary])
                    .validate_selection_with_status("work", None, "deployment")
                    .is_ok(),
                expected
            );
        };
        assert_policy(false);
        source.unavailable("work", "server returned 503 for /models".into());
        assert_policy(false);
        source.unavailable("work", "model catalog discovery timed out".into());
        assert_policy(false);
        registry.clear_discovered_models("work");
        assert_policy(false);
        source.replace(
            "work".into(),
            ProviderInventory::Fetched {
                models: vec![discovered("deployment", true, None)],
                fetched_at_ms: 1,
            },
        );
        assert_policy(true);
        assert!(
            registry
                .summary("work", &|_| true)
                .expect("stale")
                .availability_reason
                .expect("visible reason")
                .contains("refresh")
        );
        source.unavailable("work", "server returned 503 for /models".into());
        assert_policy(true);
        source.unavailable("work", "model catalog discovery timed out".into());
        assert_policy(true);
        registry.models_rejected("work", "model catalog authentication failed".into());
        assert_policy(false);
        source.unavailable(
            "work",
            "later timeout is not proof the credential recovered".into(),
        );
        assert_policy(false);
        registry.models_unsupported(
            "work",
            "later unsupported listing is not an auth success".into(),
        );
        assert_policy(false);
        source.replace(
            "work".into(),
            ProviderInventory::Fetched {
                models: vec![discovered("other", true, None)],
                fetched_at_ms: u64::MAX,
            },
        );
        assert_policy(false);
        source.replace(
            "work".into(),
            ProviderInventory::Fetched {
                models: vec![discovered("other", true, None)],
                fetched_at_ms: 1,
            },
        );
        assert_policy(true);
        assert!(
            registry
                .summary("work", &|_| true)
                .expect("stale row")
                .models
                .contains(&"deployment".into())
        );
        registry.models_unsupported("work", "server returned 405 for /models".into());
        assert_policy(true);
    }
}

#[test]
fn aged_snapshot_admits_configured_model_before_the_next_registry_publication() {
    for models in [vec![discovered("other", true, None)], vec![]] {
        let mut profile = removable_provider_profile_for_catalog_test();
        profile.configured_models = vec!["deployment".into()];
        // Exercise the authoritative membership veto as well as availability.
        profile.provenance = ProviderProvenance::BuiltIn;
        let registry = ProviderRegistry::new(
            MemoryProviderStore::default(),
            vec![profile],
            model_source([("work", models)]),
        )
        .expect("registry");
        let mut summary = registry.summary("work", &|_| true).expect("fresh summary");
        summary.inventory = haider_rpc::ModelInventoryWire::Fetched { fetched_at_ms: 100 };
        let fresh = crate::model_select::ModelSelectionAuthority::new(None, vec![summary.clone()]);
        assert!(
            fresh
                .validate_selection_with_status("work", None, "deployment")
                .is_err()
        );
        summary.project_inventory_at_time(100 + haider_rpc::MODEL_INVENTORY_TTL_MS, &[]);
        let aged = crate::model_select::ModelSelectionAuthority::new(None, vec![summary]);
        assert!(
            aged.validate_selection_with_status("work", None, "deployment")
                .is_ok()
        );
        assert!(
            aged.validate_selection_with_status("work", None, "unconfigured")
                .is_err()
        );
    }
}

#[test]
fn configured_projection_policy_is_independent_of_provider_catalog_kind() {
    for provider in [
        "work",
        HAIDER_CODE_PROVIDER_NAME,
        DEEPSEEK_PROVIDER_NAME,
        GEMINI_PROVIDER_NAME,
        ANTHROPIC_PROVIDER_NAME,
        OPENAI_PROVIDER_NAME,
        BEDROCK_PROVIDER_NAME,
    ] {
        let profile = if provider == "work" {
            removable_provider_profile_for_catalog_test()
        } else {
            builtin_or_unknown(provider, "unused")
        };
        let offline = offline_inventory(&profile);
        let unsupported = catalog_source_for_profile(&profile).is_none();
        let requires_credential = profile.auth_requirement != ProviderAuthRequirementWire::None;
        let source = model_source([]);
        let mut registry = ProviderRegistry::new(
            MemoryProviderStore::default(),
            vec![profile],
            source.clone(),
        )
        .expect("registry");
        // Configure through the production path after startup's deliberate
        // cleanup of legacy release-owned fetched catalog rows.
        registry
            .configure_with_inventory(
                ProviderConfigureInput {
                    provider: provider.into(),
                    api_family: None,
                    origin: None,
                    auth_requirement: None,
                    enabled: true,
                    models: vec!["deployment".into()],
                    default_model: Some("deployment".into()),
                    response_open_timeout_ms: None,
                    chunk_idle_timeout_ms: None,
                    semantic_progress_timeout_ms: None,
                    trust: None,
                },
                &["deployment".into()],
            )
            .expect("explicit configuration");
        let assert_policy = |expected| {
            let summary = registry.summary(provider, &|_| true).expect("summary");
            assert_eq!(
                summary.model_is_selectable("deployment"),
                expected,
                "{provider}: {summary:?}"
            );
            assert_eq!(
                crate::model_select::ModelSelectionAuthority::new(None, vec![summary])
                    .validate_selection_with_status(provider, None, "deployment")
                    .is_ok(),
                expected,
                "new-session policy for {provider}"
            );
        };
        assert_policy(offline || unsupported);
        source.unavailable(provider, "server returned 503 for /models".into());
        assert_policy(offline || unsupported);
        let uncertainty = registry.summary(provider, &|_| true).expect("503");
        if offline {
            assert_eq!(
                uncertainty.inventory,
                haider_rpc::ModelInventoryWire::Static
            );
        } else {
            assert!(uncertainty.selection_reason(None).is_some());
            if unsupported {
                assert!(
                    uncertainty
                        .availability_reason
                        .expect("reason")
                        .contains("configured (unverified)")
                );
            }
        }
        registry.models_rejected(provider, "synthetic credential rejection".into());
        assert_policy(false);
        registry.clear_discovered_models(provider);
        registry.models_unsupported(provider, "server returned 404 for /models".into());
        assert_policy(true);
        if !offline {
            let summary = registry.summary(provider, &|_| true).expect("unverified");
            assert_eq!(
                summary.inventory_authority,
                haider_rpc::ModelInventoryAuthorityWire::Advisory
            );
            assert!(
                summary
                    .availability_reason
                    .expect("reason")
                    .contains("configured (unverified)")
            );
        }
        for (model, age, selectable) in [
            ("deployment", u64::MAX, true),
            ("deployment", 1, true),
            ("other", u64::MAX, false),
            ("other", 1, true),
        ] {
            source.replace(
                provider.into(),
                ProviderInventory::Fetched {
                    models: vec![discovered(model, true, None)],
                    fetched_at_ms: age,
                },
            );
            assert_policy(selectable);
        }
        source.replace(
            provider.into(),
            ProviderInventory::Fetched {
                models: vec![discovered("deployment", true, None)],
                fetched_at_ms: u64::MAX,
            },
        );
        assert_eq!(
            registry
                .summary(provider, &|_| false)
                .expect("no credential")
                .model_is_selectable("deployment"),
            !requires_credential,
            "credential failure policy for {provider}"
        );
    }
}

#[test]
fn catalog_rejection_survives_restart_and_uncertainty_only_for_its_owner() {
    let dir = tempfile::tempdir().expect("profile");
    let mut profile = removable_provider_profile_for_catalog_test();
    profile.configured_models = vec!["deployment".into()];
    profile.default_model = Some("deployment".into());
    let store = JsonProviderRegistryStore::new(dir.path());
    store.save(&[profile.clone()]).expect("profile");
    let registry =
        ProviderRegistry::new(store.clone(), vec![], model_source([])).expect("registry");
    registry
        .record_catalog_rejection("work", "owner-A", "synthetic authentication failed".into())
        .expect("negative evidence");
    store
        .save(&[profile])
        .expect("management save preserves evidence");
    let reloaded = ProviderRegistry::new(store.clone(), vec![], model_source([])).expect("restart");
    assert!(
        !reloaded
            .restore_catalog_evidence("work", "owner-B")
            .expect("other owner")
    );
    reloaded.models_unsupported("work", "server returned 404 for /models".into());
    assert!(
        reloaded
            .summary("work", &|_| true)
            .expect("B")
            .model_is_selectable("deployment")
    );
    assert!(
        reloaded
            .restore_catalog_evidence("work", "owner-A")
            .expect("owner")
    );
    for unsupported in [false, true] {
        if unsupported {
            reloaded.models_unsupported("work", "server returned 405 for /models".into());
        } else {
            reloaded.models_unavailable("work", "synthetic timeout / 503".into());
        }
        assert!(
            !reloaded
                .summary("work", &|_| true)
                .expect("A")
                .model_is_selectable("deployment")
        );
    }
    reloaded
        .clear_catalog_evidence("owner-A")
        .expect("positive evidence retires rejection");
    reloaded.replace_discovered_models(
        "work".into(),
        vec![discovered("deployment", true, None)],
        u64::MAX,
    );
    assert!(
        reloaded
            .summary("work", &|_| true)
            .expect("recovered")
            .model_is_selectable("deployment")
    );
    let restarted =
        ProviderRegistry::new(store.clone(), vec![], model_source([])).expect("restart recovered");
    assert!(
        !restarted
            .restore_catalog_evidence("work", "owner-A")
            .expect("cleared")
    );
    let document: serde_json::Value = serde_json::from_slice(
        &std::fs::read(dir.path().join(PROVIDERS_FILE_NAME)).expect("document"),
    )
    .expect("json");
    assert!(
        document.get("catalog_evidence").is_none(),
        "empty additive field is omitted"
    );
}

#[test]
fn unsupported_discovery_survives_restart_and_uncertainty_only_for_its_owner() {
    let dir = tempfile::tempdir().expect("profile");
    let mut profile = removable_provider_profile_for_catalog_test();
    profile.configured_models = vec!["deployment".into()];
    profile.default_model = Some("deployment".into());
    let store = JsonProviderRegistryStore::new(dir.path());
    store.save(&[profile]).expect("profile");
    let registry =
        ProviderRegistry::new(store.clone(), vec![], model_source([])).expect("registry");
    registry
        .record_catalog_unsupported("work", "owner-A", "server returned 404 for /models".into())
        .expect("unsupported");
    let restarted = ProviderRegistry::new(store, vec![], model_source([])).expect("restart");
    assert!(
        !restarted
            .restore_catalog_evidence("work", "owner-B")
            .expect("B")
    );
    restarted.models_unavailable("work", "server returned 503 for /models".into());
    assert!(
        !restarted
            .summary("work", &|_| true)
            .expect("B")
            .model_is_selectable("deployment")
    );
    assert!(
        restarted
            .restore_catalog_evidence("work", "owner-A")
            .expect("A")
    );
    for reason in ["server returned 503 for /models", "synthetic timeout"] {
        restarted.models_unavailable("work", reason.into());
        let summary = restarted.summary("work", &|_| true).expect("A");
        assert!(summary.model_is_selectable("deployment"));
        assert!(
            summary
                .availability_reason
                .expect("reason")
                .contains("configured (unverified)")
        );
    }
    restarted
        .record_catalog_rejection("work", "owner-A", "synthetic auth failure".into())
        .expect("rejection");
    restarted
        .record_catalog_unsupported("work", "owner-A", "server returned 405 for /models".into())
        .expect("later uncertainty");
    assert!(
        restarted
            .restore_catalog_evidence("work", "owner-A")
            .expect("restore negative")
    );
    assert!(
        !restarted
            .summary("work", &|_| true)
            .expect("negative")
            .model_is_selectable("deployment")
    );
}

#[test]
fn unsupported_adapters_accept_explicit_models_without_fabricating_inventory() {
    for provider in [OPENAI_PROVIDER_NAME, ANTHROPIC_PROVIDER_NAME] {
        let profile = builtin_or_unknown(provider, "unused");
        assert!(catalog_source_for_profile(&profile).is_none());
        let registry = ProviderRegistry::new(
            MemoryProviderStore::default(),
            vec![profile],
            model_source([]),
        )
        .expect("registry");
        let summary = registry.summary(provider, &|_| true).expect("healthy");
        assert!(summary.models.is_empty());
        assert!(summary.model_details.is_empty());
        assert!(summary.default_model.is_none());
        assert_eq!(
            summary.inventory,
            haider_rpc::ModelInventoryWire::Configured
        );
        assert_eq!(
            summary.inventory_authority,
            haider_rpc::ModelInventoryAuthorityWire::Advisory
        );
        assert!(summary.model_is_selectable("synthetic-explicit-model"));
        assert!(
            summary
                .availability_reason
                .as_deref()
                .expect("reason")
                .contains("configured (unverified)")
        );
        assert!(
            crate::model_select::ModelSelectionAuthority::new(None, vec![summary])
                .validate_selection_with_status(provider, None, "synthetic-explicit-model")
                .is_ok()
        );
        assert!(
            !registry
                .summary(provider, &|_| false)
                .expect("credential negative")
                .model_is_selectable("synthetic-explicit-model")
        );
        assert!(
            registry
                .validate_default_model(provider, "synthetic-explicit-model")
                .is_err(),
            "an explicit passthrough must not fabricate a picker/default row"
        );
    }
}
