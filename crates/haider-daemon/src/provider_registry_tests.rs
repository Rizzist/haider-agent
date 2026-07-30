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
    )
    .expect("registry");
    let summary = registry.summaries().into_iter().next().expect("summary");
    assert_eq!(summary.availability, ProviderAvailabilityWire::Unavailable);
    assert!(!summary.enabled);
    assert!(summary.models.is_empty());
    assert_eq!(summary.default_model, None);
}

/// MUTATION CHECK: remove the membership check in `validated_default`.
/// Expected runtime failure: configuring model B as the default for the
/// model-A-only inventory returns `Ok` instead of `invalid_argument`.
#[test]
fn registered_model_invariant_rejects_an_out_of_inventory_default() {
    let store = MemoryProviderStore::default();
    let mut registry = ProviderRegistry::new(store, Vec::new()).expect("empty provider registry");
    let error = registry
        .configure(ProviderConfigureInput {
            provider: "custom".to_owned(),
            api_family: Some(ProviderApiFamilyWire::OpenAiChatCompletions),
            origin: Some("https://models.example.com".to_owned()),
            auth_requirement: Some(ProviderAuthRequirementWire::ApiKey),
            enabled: true,
            models: vec!["model-a".to_owned()],
            default_model: Some("model-b".to_owned()),
        })
        .expect_err("unregistered default must fail");
    assert_eq!(error.code, ErrorCode::InvalidArgument);
}

/// MUTATION CHECK: make `require_matching_identity` return `Ok(())`
/// unconditionally. Expected runtime failure: the existing custom provider
/// accepts the retargeting request instead of returning `invalid_argument`.
#[test]
fn existing_custom_provider_identity_fields_are_create_only() {
    let store = MemoryProviderStore::default();
    let mut registry = ProviderRegistry::new(store, Vec::new()).expect("empty provider registry");
    registry
        .configure(ProviderConfigureInput {
            provider: "custom".to_owned(),
            api_family: Some(ProviderApiFamilyWire::OpenAiChatCompletions),
            origin: Some("https://models.example.com".to_owned()),
            auth_requirement: Some(ProviderAuthRequirementWire::ApiKey),
            enabled: true,
            models: vec!["model-a".to_owned()],
            default_model: Some("model-a".to_owned()),
        })
        .expect("create custom");
    let error = registry
        .configure(ProviderConfigureInput {
            provider: "custom".to_owned(),
            api_family: None,
            origin: Some("https://attacker.example".to_owned()),
            auth_requirement: None,
            enabled: true,
            models: vec!["model-a".to_owned()],
            default_model: Some("model-a".to_owned()),
        })
        .expect_err("identity mutation must fail");
    assert!(error.message.contains("create-only"));
}
