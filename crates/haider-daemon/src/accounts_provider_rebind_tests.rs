#![allow(clippy::expect_used)]
use super::accounts_tests::{adapter_cache_descriptor, adapter_cache_profile};
use super::*;
use crate::worker::ProviderFactory as _;

#[derive(Debug, Clone, PartialEq, Eq)]
struct RebindBuildCoordinates {
    provider: String,
    profile_endpoint: Option<String>,
    descriptor_endpoint: Option<String>,
    account: CredentialAlias,
}

#[derive(Default)]
struct RebindRecordingBuilder {
    production: ProductionAccountBuilder,
    calls: StdMutex<Vec<RebindBuildCoordinates>>,
}

impl AccountProviderBuilder for RebindRecordingBuilder {
    fn providers(&self) -> std::collections::BTreeSet<String> {
        ["rebind-proxy-a".to_owned(), "rebind-proxy-b".to_owned()].into()
    }

    fn build(
        &self,
        provider: &str,
        credential: haider_accounts::SecretHandle,
        model: &str,
        alias: &CredentialAlias,
    ) -> Result<Arc<dyn Provider>, HaiderError> {
        self.production.build(provider, credential, model, alias)
    }

    #[allow(clippy::too_many_arguments)]
    fn build_tuned_with_cache(
        &self,
        profile: Option<&ProviderSummaryWire>,
        descriptor: &CredentialDescriptor,
        credential: haider_accounts::SecretHandle,
        model: &str,
        tuning: &ProviderTuning,
        catalog_model: Option<&DiscoveredModel>,
        gemini_cache_registry: Arc<haider_provider::GeminiCacheRegistry>,
    ) -> Result<Arc<dyn Provider>, HaiderError> {
        self.calls
            .lock()
            .expect("record build")
            .push(RebindBuildCoordinates {
                provider: descriptor.provider.clone(),
                profile_endpoint: profile.and_then(|profile| profile.endpoint.clone()),
                descriptor_endpoint: descriptor.base_url.clone(),
                account: descriptor.alias.clone(),
            });
        self.production.build_tuned_with_cache(
            profile,
            descriptor,
            credential,
            model,
            tuning,
            catalog_model,
            gemini_cache_registry,
        )
    }
}

struct RebindFactoryFixture {
    factory: AccountsProviderFactory,
    builder: Arc<RebindRecordingBuilder>,
    management: ManagementSnapshot,
    descriptors: AccountsSnapshot,
}

fn rebind_factory_fixture() -> RebindFactoryFixture {
    let mut selected = adapter_cache_descriptor("rebind-proxy-a", "selected-a");
    selected.active = false;
    selected.base_url = Some("http://127.0.0.1:31401/v1".into());
    let active = CredentialDescriptor {
        alias: CredentialAlias::new("active-a"),
        active: true,
        ..selected.clone()
    };
    let mut other = adapter_cache_descriptor("rebind-proxy-b", "selected-b");
    other.base_url = Some("http://127.0.0.1:31402/v1".into());
    let descriptors = vec![active, selected, other];
    let vault = Arc::new(MemoryVault::default());
    for descriptor in &descriptors {
        vault
            .put(&descriptor.alias, b"rebind-test-key")
            .expect("seed credential");
    }
    let management = ManagementSnapshot::new(
        1,
        descriptors.clone(),
        vec![
            adapter_cache_profile("rebind-proxy-a", "http://127.0.0.1:31501/v1"),
            adapter_cache_profile("rebind-proxy-b", "http://127.0.0.1:31502/v1"),
        ],
    );
    let descriptors = Arc::new(StdMutex::new(descriptors));
    let builder = Arc::new(RebindRecordingBuilder::default());
    let factory = AccountsProviderFactory::new_with_management(
        descriptors.clone(),
        management.clone(),
        VaultProvision::Available(vault),
        builder.clone(),
    );
    RebindFactoryFixture {
        factory,
        builder,
        management,
        descriptors,
    }
}

fn rebind_metadata() -> haider_protocol::session::SessionMetadataV1 {
    serde_json::from_value(serde_json::json!({
        "cwd": "/tmp",
        "provider": "rebind-proxy-a",
        "account_alias": "selected-a",
        "model": "adapter-cache-model",
        "max_tokens": 64,
        "created_at_ms": 1,
    }))
    .expect("metadata")
}

/// MUTATION CHECK: override only the descriptor or only the registry profile.
/// Both construction coordinates must name the one session's proxy endpoint.
#[tokio::test]
async fn provider_rebind_endpoint_overrides_both_coordinates_without_global_mutation() {
    let fixture = rebind_factory_fixture();
    let before = fixture.management.read().expect("management");
    let descriptors_before = fixture.descriptors.lock().expect("descriptors").clone();
    let mut metadata = rebind_metadata();
    metadata.provider_base_url = Some("http://127.0.0.1:31601/v1".into());
    metadata.provider_rebind_id = Some("rebind-a".into());
    let selected = fixture
        .factory
        .resolve_for_turn(&metadata)
        .await
        .expect("resolve override");
    assert_eq!(selected.account_alias.as_deref(), Some("selected-a"));
    assert_eq!(
        fixture.builder.calls.lock().expect("calls").as_slice(),
        &[RebindBuildCoordinates {
            provider: "rebind-proxy-a".into(),
            profile_endpoint: metadata.provider_base_url.clone(),
            descriptor_endpoint: metadata.provider_base_url,
            account: CredentialAlias::new("selected-a"),
        },]
    );
    let after = fixture.management.read().expect("management");
    assert_eq!(after.providers, before.providers);
    assert_eq!(after.descriptors, before.descriptors);
    assert_eq!(
        *fixture.descriptors.lock().expect("descriptors"),
        descriptors_before
    );
}

#[tokio::test]
async fn provider_rebind_cache_reuses_same_route_and_separates_changed_endpoint() {
    let fixture = rebind_factory_fixture();
    let original_metadata = rebind_metadata();
    let original = fixture
        .factory
        .resolve_for_turn(&original_metadata)
        .await
        .expect("original");
    let mut metadata = original_metadata.clone();
    metadata.provider_base_url = Some("http://127.0.0.1:31601/v1".into());
    let first = fixture
        .factory
        .resolve_for_turn(&metadata)
        .await
        .expect("first route");
    let same = fixture
        .factory
        .resolve_for_turn(&metadata)
        .await
        .expect("same route");
    assert!(Arc::ptr_eq(&first.provider, &same.provider));
    assert!(!Arc::ptr_eq(&first.provider, &original.provider));
    metadata.provider_base_url = Some("http://127.0.0.1:31602/v1".into());
    let second = fixture
        .factory
        .resolve_for_turn(&metadata)
        .await
        .expect("second route");
    assert!(!Arc::ptr_eq(&first.provider, &second.provider));
    let unchanged = fixture
        .factory
        .resolve_for_turn(&original_metadata)
        .await
        .expect("original remains");
    assert!(Arc::ptr_eq(&unchanged.provider, &original.provider));
    assert_eq!(
        fixture
            .builder
            .production
            .cached_adapter_count()
            .expect("cache count"),
        3
    );
}

#[tokio::test]
async fn provider_rebind_selected_unknown_or_other_provider_account_fails_before_build() {
    let fixture = rebind_factory_fixture();
    let mut metadata = rebind_metadata();
    metadata.provider_base_url = Some("http://127.0.0.1:31601/v1".into());
    metadata.account_alias = Some("missing-account".into());
    let missing = fixture
        .factory
        .resolve_for_turn(&metadata)
        .await
        .err()
        .expect("missing account");
    assert_eq!(missing.code, ErrorCode::CredentialMissing);
    metadata.account_alias = Some("selected-b".into());
    let wrong_provider = fixture
        .factory
        .resolve_for_turn(&metadata)
        .await
        .err()
        .expect("wrong provider");
    assert_eq!(wrong_provider.code, ErrorCode::InvalidArgument);
    assert!(fixture.builder.calls.lock().expect("calls").is_empty());
}

#[tokio::test]
async fn provider_rebind_two_registered_custom_providers_use_their_selected_accounts() {
    let fixture = rebind_factory_fixture();
    let mut metadata = rebind_metadata();
    metadata.provider_base_url = Some("http://127.0.0.1:31601/v1".into());
    let first = fixture
        .factory
        .resolve_for_turn(&metadata)
        .await
        .expect("first provider");
    metadata.provider = "rebind-proxy-b".into();
    metadata.account_alias = Some("selected-b".into());
    metadata.provider_base_url = Some("http://127.0.0.1:31602/v1".into());
    let second = fixture
        .factory
        .resolve_for_turn(&metadata)
        .await
        .expect("second provider");
    assert_eq!(first.provider_name, "rebind-proxy-a");
    assert_eq!(first.account_alias.as_deref(), Some("selected-a"));
    assert_eq!(second.provider_name, "rebind-proxy-b");
    assert_eq!(second.account_alias.as_deref(), Some("selected-b"));
    assert!(!Arc::ptr_eq(&first.provider, &second.provider));
    let calls = fixture.builder.calls.lock().expect("calls");
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[0].profile_endpoint.as_deref(),
        Some("http://127.0.0.1:31601/v1")
    );
    assert_eq!(
        calls[1].profile_endpoint.as_deref(),
        Some("http://127.0.0.1:31602/v1")
    );
}
