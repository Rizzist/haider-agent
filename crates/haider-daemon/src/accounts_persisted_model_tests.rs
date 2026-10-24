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

#[test]
fn automatic_selection_rederives_fetched_settings_and_preserves_explicit_budget() {
    use haider_protocol::output_budget::SessionOutputBudgetSourceV1::{Derived, UserSet};
    let mut profile = summary(ModelInventoryWire::Fetched { fetched_at_ms: 1 });
    profile.catalog = ProviderCatalogKindWire::Authenticated;
    profile.configured_models = profile.models.clone();
    profile.catalog_scope = Some(haider_rpc::ProviderCatalogScopeWire {
        key: "synthetic-target-B".into(),
        account_alias: Some("work".into()),
        account_incarnation: Some(2),
    });
    profile.model_details = vec![haider_rpc::ModelDetailWire {
        source: Some(haider_rpc::ModelDetailSourceWire::Remote),
        name: profile.models[0].clone(),
        display_name: None,
        context_window: Some(100_000),
        max_output_tokens: Some(500),
        supported_efforts: Vec::new(),
        default_effort: None,
        supported_speeds: Vec::new(),
        supports_thinking_type: None,
        supports_vision: None,
    }];
    for (source, expected, owner) in [
        (Derived, 500, Some("synthetic-target-B")),
        (UserSet { requested: 1000 }, 500, Some("synthetic-target-B")),
        (UserSet { requested: 100 }, 100, None),
    ] {
        let mut metadata = old_metadata();
        metadata.max_tokens = 111;
        metadata.max_tokens_source = Some(source);
        metadata.model_catalog_key = Some("synthetic-source-A".into());
        let (key, budget) = AccountsProviderFactory::automatic_model_selection(
            &profile,
            &profile.models[0],
            &metadata,
        )
        .expect("target selection");
        assert_eq!(budget.max_tokens, expected);
        assert_eq!(budget.source, source, "the requested source remains intact");
        assert_eq!(
            key.as_deref(),
            owner,
            "A's effective default is not B's state"
        );
    }
}

fn summary(inventory: ModelInventoryWire) -> ProviderSummaryWire {
    ProviderSummaryWire {
        catalog_scope: None,
        configured_models: Vec::new(),
        static_models: Vec::new(),
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
        incarnation: 0,
        credential_digest: None,
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
        exercise_provider_round(&turn.provider, &turn.model).await;
    }
}

async fn exercise_provider_round(provider: &Arc<dyn Provider>, model: &str) {
    let _ = provider
        .stream_turn(TurnRequest {
            messages: Vec::new(),
            model: model.into(),
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

/// Explicit names remain configuration, but their discovery admission cannot
/// be borrowed from an inventory published before a credential/route change.
#[tokio::test]
async fn explicit_model_cannot_borrow_retired_inventory_eligibility() {
    for azure in [false, true] {
        for repoint in [false, true] {
            let mut profile = summary(ModelInventoryWire::Fetched { fetched_at_ms: 1 });
            profile.catalog = ProviderCatalogKindWire::Authenticated;
            profile.models = vec!["Go".into()];
            if azure {
                profile.catalog = ProviderCatalogKindWire::Custom;
                profile.inventory_authority = ModelInventoryAuthorityWire::Advisory;
                profile.endpoint = Some("https://synthetic.openai.azure.com".into());
                profile.configured_models = profile.models.clone();
                profile.inventory = ModelInventoryWire::Configured;
                profile.availability_reason = Some("synthetic listing HTTP 404".into());
            }
            profile.catalog_scope = Some(haider_rpc::ProviderCatalogScopeWire {
                key: "pending".into(),
                account_alias: Some("upgrade-fixture".into()),
                account_incarnation: Some(0),
            });
            let (factory, provider, alias) = factory(profile.clone());
            let mut descriptor = factory.snapshot.lock().expect("snapshot")[0].clone();
            profile.catalog_scope.as_mut().expect("scope").key =
                AccountsProviderFactory::descriptor_catalog_key(&profile, Some(&descriptor))
                    .expect("A's producer");
            let management = factory.management.as_ref().expect("management");
            management.publish(2, vec![descriptor.clone()], vec![profile.clone()]);
            let mut metadata = old_metadata();
            metadata.account_alias = Some(alias.as_str().into());
            assert_eq!(metadata.model_catalog_key, None, "explicit configuration");
            let admitted = factory.resolve_for_turn(&metadata).await;
            exercise_admitted_turn(&admitted).await;
            assert!(admitted.is_ok(), "unchanged producer remains available");
            assert_eq!(provider.requests().len(), 1);

            if repoint {
                metadata.provider_base_url = Some("http://127.0.0.1:23456/v1".into());
            } else {
                descriptor.incarnation = 1;
                *factory.snapshot.lock().expect("snapshot") = vec![descriptor.clone()];
            }
            // Mirror the real writer's account-before-inventory publish order.
            for resumed in [false, true] {
                let result = if resumed {
                    factory
                        .resolve_for_turn_with_web(&metadata, Default::default())
                        .await
                } else {
                    factory.resolve_for_turn(&metadata).await
                };
                exercise_admitted_turn(&result).await;
                let error = result.err().expect("retired eligibility refused");
                assert_eq!(error.code, ErrorCode::ProviderError);
                assert!(error.retryable);
                assert!(error.message.contains("different account or endpoint"));
                assert_eq!(
                    provider.requests().len(),
                    1,
                    "no round using A's eligibility"
                );
            }
            // A fresh listing for B (or the new route) restores availability.
            if let Some(origin) = &metadata.provider_base_url {
                profile.endpoint = Some(origin.clone());
                descriptor.base_url = Some(origin.clone());
                descriptor.incarnation = 1;
                *factory.snapshot.lock().expect("snapshot") = vec![descriptor.clone()];
            }
            profile.catalog_scope.as_mut().expect("scope").key =
                AccountsProviderFactory::descriptor_catalog_key(&profile, Some(&descriptor))
                    .expect("replacement producer");
            profile
                .catalog_scope
                .as_mut()
                .expect("scope")
                .account_incarnation = Some(descriptor.incarnation);
            management.publish(3, vec![descriptor], vec![profile]);
            let refreshed = factory.resolve_for_turn(&metadata).await;
            exercise_admitted_turn(&refreshed).await;
            assert!(
                refreshed.is_ok(),
                "the model remains explicit configuration"
            );
            assert_eq!(provider.requests().len(), 2);
        }
    }
}

#[tokio::test]
async fn explicit_model_checks_resolved_credential_before_adapter_construction() {
    for kind in ["authoritative", "azure", "advisory"] {
        let mut profile = summary(ModelInventoryWire::Fetched { fetched_at_ms: 1 });
        profile.catalog = ProviderCatalogKindWire::Authenticated;
        profile.models = vec!["Go".into()];
        if kind != "authoritative" {
            profile.catalog = ProviderCatalogKindWire::Custom;
            profile.inventory_authority = ModelInventoryAuthorityWire::Advisory;
            profile.configured_models = profile.models.clone();
        }
        if kind == "azure" {
            profile.endpoint = Some("https://synthetic.openai.azure.com".into());
            profile.inventory = ModelInventoryWire::Configured;
            profile.availability_reason = Some("synthetic listing HTTP 405".into());
        }
        profile.catalog_scope = Some(haider_rpc::ProviderCatalogScopeWire {
            key: "pending".into(),
            account_alias: Some("upgrade-fixture".into()),
            account_incarnation: Some(1),
        });
        let (factory, provider, alias) = factory(profile.clone());
        let previous = factory.snapshot.lock().expect("snapshot")[0].clone();
        let mut current = previous.clone();
        current.incarnation = 1;
        *factory.snapshot.lock().expect("snapshot") = vec![current.clone()];
        profile.catalog_scope.as_mut().expect("scope").key =
            AccountsProviderFactory::descriptor_catalog_key(&profile, Some(&current))
                .expect("B's current producer");
        let endpoint = profile.endpoint.clone();
        factory
            .management
            .as_ref()
            .expect("management")
            .publish(2, vec![current], vec![profile]);
        let metadata = old_metadata();
        assert!(factory.admit_persisted_model(&metadata).is_ok());
        let vault = MemoryVault::default();
        vault
            .put(&alias, b"synthetic-resolved-key")
            .expect("test key");
        // The credential was resolved before B's current catalog was published.
        let result = factory.build_provider(
            &previous,
            vault.resolve(&alias).expect("test handle"),
            &metadata,
            &ProviderTuning::default(),
        );
        if let Ok(adapter) = &result {
            exercise_provider_round(adapter, &metadata.model).await;
        }
        let error = result.err().expect("cannot borrow B's eligibility for A");
        assert_eq!(error.code, ErrorCode::ProviderError);
        assert!(error.retryable);
        assert!(provider.requests().is_empty());
        let fresh = factory.resolve_for_turn(&metadata).await;
        exercise_admitted_turn(&fresh).await;
        assert!(fresh.is_ok(), "matching resolved credential remains usable");
        assert_eq!(provider.requests().len(), 1);
        // Spelling the already-current effective origin explicitly does not
        // create a different credential or invalidate its discovery result.
        let mut same_route = metadata;
        same_route.provider_base_url = endpoint;
        let routed = factory.resolve_for_turn(&same_route).await;
        exercise_admitted_turn(&routed).await;
        assert!(routed.is_ok(), "same effective origin remains available");
        assert_eq!(provider.requests().len(), 2);
    }
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

/// A session default is derived data even on an advisory custom provider.
/// Same-alias replacement must refuse it before constructing/sending a turn.
#[tokio::test]
async fn catalog_derived_session_selection_keeps_its_original_incarnation() {
    let mut profile = summary(ModelInventoryWire::Fetched { fetched_at_ms: 1 });
    profile.provider = "custom-fixture".into();
    profile.catalog = ProviderCatalogKindWire::Custom;
    profile.inventory_authority = ModelInventoryAuthorityWire::Advisory;
    profile.models = vec!["Go".into()];
    profile.default_model = Some("Go".into());
    profile.catalog_scope = Some(haider_rpc::ProviderCatalogScopeWire {
        key: "pending".into(),
        account_alias: Some("upgrade-fixture".into()),
        account_incarnation: Some(0),
    });
    let (factory, provider, alias) = factory(profile.clone());
    let mut descriptor = factory.snapshot.lock().expect("snapshot")[0].clone();
    let original_key = AccountsProviderFactory::descriptor_catalog_key(&profile, Some(&descriptor))
        .expect("private producer");
    profile.catalog_scope.as_mut().expect("scope").key = original_key.clone();
    factory.management.as_ref().expect("management").publish(
        2,
        vec![descriptor.clone()],
        vec![profile.clone()],
    );
    let mut metadata = old_metadata();
    metadata.provider = profile.provider.clone();
    metadata.model_catalog_key = Some(original_key.clone());
    metadata.account_alias = Some(alias.as_str().into());
    // Ordinary refresh/token rotation of the same incarnation is continuous.
    let admitted = factory.resolve_for_turn(&metadata).await;
    exercise_admitted_turn(&admitted).await;
    assert!(admitted.is_ok());
    assert_eq!(provider.requests().len(), 1);

    descriptor.incarnation = 1;
    *factory.snapshot.lock().expect("snapshot") = vec![descriptor.clone()];
    let replacement_key =
        AccountsProviderFactory::descriptor_catalog_key(&profile, Some(&descriptor))
            .expect("replacement producer");
    profile.catalog_scope.as_mut().expect("scope").key = replacement_key.clone();
    profile
        .catalog_scope
        .as_mut()
        .expect("scope")
        .account_incarnation = Some(1);
    factory.management.as_ref().expect("management").publish(
        3,
        vec![descriptor.clone()],
        vec![profile.clone()],
    );
    haider_protocol::session::SessionProviderRebound {
        rebind_id: "synthetic-rebind".into(),
        provider: profile.provider.clone(),
        base_url: None,
        account: Some(alias.as_str().into()),
        selection_epoch: None,
    }
    .apply_to_metadata(&mut metadata);
    assert_eq!(
        metadata.model_catalog_key.as_deref(),
        Some(original_key.as_str()),
        "a route edit cannot erase the producer of derived model settings"
    );
    let retired = factory.resolve_for_turn(&metadata).await;
    exercise_admitted_turn(&retired).await;
    let error = retired.err().expect("retired derived selection");
    assert_eq!(error.code, ErrorCode::InvalidArgument);
    assert_eq!(
        error.details.expect("reselection")["requires_model_reselection"],
        true
    );
    assert_eq!(
        provider.requests().len(),
        1,
        "no physical request using A's default under B"
    );

    metadata.model_catalog_key = Some(replacement_key);
    let reselected = factory.resolve_for_turn(&metadata).await;
    exercise_admitted_turn(&reselected).await;
    assert!(reselected.is_ok());
    assert_eq!(provider.requests().len(), 2);
    // A repoint changes the producer even when the account coordinate stays.
    profile.endpoint = Some("http://127.0.0.1:23456/v1".into());
    factory
        .management
        .as_ref()
        .expect("management")
        .publish(4, vec![descriptor], vec![profile]);
    let repointed = factory.resolve_for_turn(&metadata).await;
    assert!(repointed.is_err());
    assert_eq!(provider.requests().len(), 2);
    // Explicit unlisted configuration remains advisory passthrough.
    metadata.model_catalog_key = None;
    metadata.model = "manually-entered-unlisted-model".into();
    let manual = factory.resolve_for_turn(&metadata).await;
    exercise_admitted_turn(&manual).await;
    assert!(manual.is_ok());
    assert_eq!(provider.requests().len(), 3);
}

#[test]
fn automatic_routing_owner_includes_anonymous_endpoint_context() {
    let mut profile = summary(ModelInventoryWire::Fetched { fetched_at_ms: 1 });
    profile.provider = "custom-fixture".into();
    profile.catalog = ProviderCatalogKindWire::Custom;
    profile.inventory_authority = ModelInventoryAuthorityWire::Advisory;
    profile.auth_methods.clear();
    profile.models = vec!["same-model-name".into()];
    profile.catalog_scope = Some(haider_rpc::ProviderCatalogScopeWire {
        key: "pending".into(),
        account_alias: None,
        account_incarnation: None,
    });
    profile.catalog_scope.as_mut().expect("scope").key =
        AccountsProviderFactory::descriptor_catalog_key(&profile, None)
            .expect("anonymous producer");
    let (factory, _, _) = factory(profile.clone());
    let factory = factory.with_resilience(AccountsResilienceConfig {
        fallback_chain: vec![ProviderTargetV1 {
            provider: profile.provider.clone(),
            model: Some("same-model-name".into()),
        }],
        promotion_targets: HashMap::new(),
    });
    assert!(factory.resilience_model_is_current(&profile, "same-model-name"));
    profile.endpoint = Some("http://127.0.0.1:23457/v1".into());
    profile.catalog_scope.as_mut().expect("scope").key =
        AccountsProviderFactory::descriptor_catalog_key(&profile, None)
            .expect("new origin producer");
    factory.management.as_ref().expect("management").publish(
        2,
        factory.snapshot.lock().expect("snapshot").clone(),
        vec![profile.clone()],
    );
    assert!(
        !factory.resilience_model_is_current(&profile, "same-model-name"),
        "the old automatic choice is retired even when B lists the same spelling"
    );
    // Explicit configured deployments are independent of remote ownership.
    let detail = haider_rpc::ModelDetailWire {
        source: Some(haider_rpc::ModelDetailSourceWire::Configured),
        name: "same-model-name".into(),
        display_name: None,
        context_window: None,
        max_output_tokens: None,
        supported_efforts: Vec::new(),
        default_effort: None,
        supported_speeds: Vec::new(),
        supports_thinking_type: None,
        supports_vision: None,
    };
    profile.model_details = vec![detail];
    assert!(factory.resilience_model_is_current(&profile, "same-model-name"));
}

#[tokio::test]
async fn c5_recorded_creation_coordinate_admits_the_real_canonical_provider_request() {
    for (spelling, later_ambiguity) in [
        ("DEEPSEEK_V4_FLASH", false),
        (" DeepSeek V4 Flash ", false),
        ("deepseek\u{a0}v4\u{2003}flash", false),
        ("DEEPSEEK_V4_FLASH", true),
        (" DeepSeek V4 Flash ", true),
        ("deepseek\u{a0}v4\u{2003}flash", true),
    ] {
        let mut profile = summary(ModelInventoryWire::Fetched { fetched_at_ms: 1 });
        if later_ambiguity {
            // The saved coordinate names a specific previously admitted row.
            // Later alias collisions must not reinterpret that selection.
            profile.models.push("deepseek_v4_flash".into());
        }
        let (factory, provider, _) = factory(profile);
        // JSON keeps this a runtime regression on the old decoder too: an
        // unknown additive field is ignored there and admission still fails.
        let mut json = serde_json::to_value(old_metadata()).expect("metadata");
        json["model"] = serde_json::json!(spelling);
        json["resolved_model"] = serde_json::json!("deepseek-v4-flash");
        let metadata = serde_json::from_value(json).expect("recorded creation");
        let result = factory.resolve_for_turn(&metadata).await;
        exercise_admitted_turn(&result).await;
        assert_eq!(
            result.expect("C5 recorded spelling must be admitted").model,
            "deepseek-v4-flash"
        );
        assert_eq!(provider.requests().len(), 1);
        assert_eq!(provider.requests()[0].model, "deepseek-v4-flash");
    }
}
