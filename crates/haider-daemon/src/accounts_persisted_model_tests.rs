#![allow(clippy::expect_used)]
//! Upgrade admission uses actual release metadata, plus a credentialed recording
//! provider so removing the fence causes a measurable provider round.

use super::*;
use crate::worker::ProviderFactory as _;
use haider_core::ProviderAttemptResolver as _;
use haider_rpc::{ModelInventoryAuthorityWire, ModelInventoryWire, ProviderCatalogKindWire};

fn old_metadata() -> haider_protocol::session::SessionMetadataV1 {
    serde_json::from_str(include_str!(
        "../../haider-cli/tests/fixtures/preupgrade-go-session.metadata.json"
    ))
    .expect("actual installed 0.0.971 session metadata")
}

fn summary(inventory: ModelInventoryWire) -> ProviderSummaryWire {
    ProviderSummaryWire {
        provider: HAIDER_CODE_PROVIDER_NAME.into(),
        api_family: ProviderApiFamilyWire::OpenAiChatCompletions,
        endpoint: Some(HAIDER_CODE_BASE_URL.into()),
        response_open_timeout_ms: None,
        chunk_idle_timeout_ms: None,
        semantic_progress_timeout_ms: None,
        models: vec!["deepseek-v4-flash".into()],
        model_details: Vec::new(),
        catalog: ProviderCatalogKindWire::Public,
        inventory,
        inventory_authority: ModelInventoryAuthorityWire::Authoritative,
        auth_methods: vec![AuthMethod::ApiKey],
        availability: ProviderAvailabilityWire::Available,
        availability_reason: None,
        default_model: Some("deepseek-v4-flash".into()),
        enabled: true,
        trust: ProviderTrustWire::Full,
    }
}

struct RecordingBuilder(Arc<haider_provider::FakeProvider>);

impl AccountProviderBuilder for RecordingBuilder {
    fn providers(&self) -> std::collections::BTreeSet<String> {
        std::collections::BTreeSet::from([HAIDER_CODE_PROVIDER_NAME.into()])
    }

    fn build(
        &self,
        _provider: &str,
        _credential: SecretHandle,
        _model: &str,
        _alias: &CredentialAlias,
    ) -> Result<Arc<dyn Provider>, HaiderError> {
        Ok(self.0.clone())
    }
}

fn factory(
    summary: ProviderSummaryWire,
) -> (
    AccountsProviderFactory,
    Arc<haider_provider::FakeProvider>,
    CredentialAlias,
) {
    let alias = CredentialAlias::new("upgrade-fixture");
    let descriptor = CredentialDescriptor {
        alias: alias.clone(),
        provider: summary.provider.clone(),
        base_url: None,
        auth_method: AuthMethod::ApiKey,
        identity: "synthetic upgrade test".into(),
        status: CredentialStatus::Ok,
        active: true,
        label: None,
        account_identity: None,
        created_at_ms: None,
    };
    let vault = Arc::new(MemoryVault::default());
    vault
        .put(&alias, b"synthetic-test-key")
        .expect("test credential");
    let provider = Arc::new(haider_provider::FakeProvider::new(Vec::new()));
    let factory = AccountsProviderFactory::new_with_management(
        Arc::new(StdMutex::new(vec![descriptor.clone()])),
        ManagementSnapshot::new(1, vec![descriptor], vec![summary]),
        VaultProvision::Available(vault),
        Arc::new(RecordingBuilder(provider.clone())),
    );
    (factory, provider, alias)
}

async fn exercise_admitted_turn(result: &Result<crate::worker::ResolvedTurnProvider, HaiderError>) {
    if let Ok(turn) = result {
        let _ = turn
            .provider
            .stream_turn(TurnRequest {
                messages: Vec::new(),
                model: turn.model.clone(),
                max_tokens: 16,
                system_prompt: None,
                tools: Vec::new(),
                attachments: Vec::new(),
                cache_metadata: None,
                tool_result_image_projection: Default::default(),
            })
            .await
            .expect("record physical provider round");
    }
}

fn assert_reselection(error: &HaiderError) {
    assert_eq!(error.code, ErrorCode::InvalidArgument);
    assert!(!error.retryable);
    assert!(error.message.contains("explicitly reselect"), "{error:?}");
    assert!(
        error.message.contains("haider models --refresh"),
        "{error:?}"
    );
    assert_eq!(
        error.details.as_ref().expect("selection coordinates")["model"],
        "Go"
    );
}

/// MUTATION: remove persisted-session admission. This release fixture must
/// then send Go to the credentialed provider and fail the ZERO-round assertion.
#[tokio::test]
async fn preupgrade_go_session_requires_reselection_with_zero_provider_rounds() {
    let metadata = old_metadata();
    assert_eq!(metadata.model, "Go");
    for inventory in [
        ModelInventoryWire::NeverFetched,
        ModelInventoryWire::Unavailable {
            reason: "offline".into(),
        },
        ModelInventoryWire::Fetched { fetched_at_ms: 1 },
        ModelInventoryWire::Stale {
            fetched_at_ms: 1,
            reason: "offline".into(),
        },
    ] {
        let mut profile = summary(inventory);
        if profile.inventory.fetched_at_ms().is_none() {
            profile.models.clear();
            profile.default_model = None;
        }
        let (factory, provider, _) = factory(profile);
        // Both factory entry points used by ordinary and restarted workers.
        let first = factory.resolve_for_turn(&metadata).await;
        exercise_admitted_turn(&first).await;
        let resumed = factory
            .resolve_for_turn_with_web(&metadata, Default::default())
            .await;
        exercise_admitted_turn(&resumed).await;
        assert_eq!(
            provider.requests().len(),
            0,
            "old Go must never reach a provider round"
        );
        assert_reselection(&first.err().expect("ordinary turn refused"));
        assert_reselection(&resumed.err().expect("resumed turn refused"));
    }
}

#[tokio::test]
async fn persisted_model_retry_checks_current_catalog_before_waiting_or_rebuilding() {
    let metadata = old_metadata();
    let mut old_catalog = summary(ModelInventoryWire::Fetched { fetched_at_ms: 1 });
    old_catalog.models = vec!["Go".into()];
    old_catalog.default_model = Some("Go".into());
    let (factory, provider, alias) = factory(old_catalog);
    factory
        .resolve_for_turn(&metadata)
        .await
        .expect("originally listed model");
    let management = factory.management.as_ref().expect("live catalog");
    let view = management.read().expect("catalog snapshot");
    management.publish(
        2,
        view.descriptors,
        vec![summary(ModelInventoryWire::Fetched { fetched_at_ms: 2 })],
    );
    let resolver =
        AccountsAttemptResolver::new(factory, metadata, ProviderTuning::default(), None, false);
    for kind in [
        ProviderErrorKind::Transport,
        ProviderErrorKind::Authentication,
        ProviderErrorKind::RateLimited,
    ] {
        let result = resolver
            .resolve(&alias, &ProviderError::new(kind, "fixture retry"))
            .await;
        assert_reselection(&result.err().expect("retry of obsolete model refused"));
    }
    assert!(provider.requests().is_empty());
}

#[tokio::test]
async fn persisted_model_admission_preserves_advisory_offline_and_listed_models() {
    for (provider_name, catalog, authority, model) in [
        (
            "custom-fixture",
            ProviderCatalogKindWire::Custom,
            ModelInventoryAuthorityWire::Advisory,
            "DeepSeek V4 Flash",
        ),
        (
            ANTHROPIC_PROVIDER_NAME,
            ProviderCatalogKindWire::Offline,
            ModelInventoryAuthorityWire::Authoritative,
            "offline-model-passthrough",
        ),
        (
            HAIDER_CODE_PROVIDER_NAME,
            ProviderCatalogKindWire::Public,
            ModelInventoryAuthorityWire::Authoritative,
            "deepseek-v4-flash",
        ),
    ] {
        let mut profile = summary(ModelInventoryWire::Fetched { fetched_at_ms: 1 });
        profile.provider = provider_name.into();
        profile.catalog = catalog;
        profile.inventory_authority = authority;
        if catalog == ProviderCatalogKindWire::Offline {
            profile.inventory = ModelInventoryWire::Static;
        }
        let (factory, provider, _) = factory(profile);
        let mut metadata = old_metadata();
        metadata.provider = provider_name.into();
        metadata.model = model.into();
        let result = factory.resolve_for_turn(&metadata).await;
        exercise_admitted_turn(&result).await;
        assert_eq!(result.expect("admitted selection").model, model);
        assert_eq!(provider.requests().len(), 1);
    }
}

#[tokio::test]
async fn persisted_custom_model_remains_admissible_without_discovery() {
    for inventory in [
        ModelInventoryWire::NeverFetched,
        ModelInventoryWire::Unavailable {
            reason: "server has no catalog".into(),
        },
    ] {
        let mut profile = summary(inventory);
        profile.provider = "custom-fixture".into();
        profile.catalog = ProviderCatalogKindWire::Custom;
        profile.inventory_authority = ModelInventoryAuthorityWire::Advisory;
        profile.models.clear();
        profile.default_model = None;
        let (factory, provider, _) = factory(profile);
        let mut metadata = old_metadata();
        metadata.provider = "custom-fixture".into();
        let result = factory.resolve_for_turn(&metadata).await;
        exercise_admitted_turn(&result).await;
        assert_eq!(result.expect("custom passthrough").model, "Go");
        assert_eq!(provider.requests().len(), 1);
    }
}

#[tokio::test]
async fn persisted_remote_model_cannot_use_empty_catalog_or_implicit_alias() {
    for models in [Vec::new(), vec!["go".into()]] {
        let mut profile = summary(ModelInventoryWire::Fetched { fetched_at_ms: 1 });
        profile.models = models;
        profile.default_model = None;
        let (factory, provider, _) = factory(profile);
        let result = factory.resolve_for_turn(&old_metadata()).await;
        exercise_admitted_turn(&result).await;
        assert_eq!(provider.requests().len(), 0);
        assert_reselection(&result.err().expect("explicit reselection required"));
    }
}
