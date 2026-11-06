//! Real account actor publication followed by account-scoped model decisions.
//! The actor constructs and publishes every shared summary. Discovery results
//! and queued raw catalog completions are injectable; summaries are not mocked.
use super::*;
use crate::session_hub::{HubConnection, SessionHub, SessionHubConfig};
use haider_protocol::ids::{DeviceId, EventId, SessionId};
use haider_rpc::{AttachMode, Capability, CommandId, RequestBody};

const PROVIDER: &str = OPENAI_OAUTH_PROVIDER_NAME;
const MODEL: &str = "account-owned-model";
const INITIAL_MODEL: &str = "gpt-6-sol";

async fn response_for(
    frames: &mut mpsc::UnboundedReceiver<WireFrame>,
    expected: &str,
) -> ResponseBody {
    loop {
        if let WireFrame::Response { request_id, body } =
            frames.recv().await.expect("correlated response")
            && request_id.as_str() == expected
        {
            return body;
        }
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    provider: &'static str,
    store: SqliteStoreHandle,
    actor: AccountActorHandle,
    hub: SessionHub,
    connection: HubConnection,
    frames: mpsc::UnboundedReceiver<WireFrame>,
    discoverer: Arc<BlockingModelDiscoverer>,
    session: SessionId,
    retired_a: Option<CredentialDescriptor>,
}

fn catalog(maximum: u64) -> DiscoveredCatalog {
    DiscoveredCatalog {
        models: vec![DiscoveredModel {
            slug: MODEL.into(),
            display_name: MODEL.into(),
            context_window: Some(maximum * 4),
            max_output_tokens: Some(maximum),
            description: None,
            default_effort: None,
            supported_efforts: Vec::new(),
            visible: true,
            priority: None,
            use_responses_lite: Some(true),
            extensions: None,
        }],
        etag: Some("owned-etag".into()),
    }
}

fn other_catalog() -> DiscoveredCatalog {
    let mut catalog = catalog(8192);
    catalog.models[0].slug = INITIAL_MODEL.into();
    catalog.models[0].display_name = INITIAL_MODEL.into();
    catalog
}

impl Fixture {
    async fn new(
        results: Vec<Result<DiscoveredCatalog, CatalogError>>,
        pin: Option<&str>,
        foreign: bool,
    ) -> Self {
        Self::start(
            results,
            pin,
            foreign,
            PROVIDER,
            AuthMethod::OAuth,
            test_provider_registry(),
            None,
        )
        .await
    }

    async fn custom(results: Vec<Result<DiscoveredCatalog, CatalogError>>) -> Self {
        Self::custom_with_recovery(results, None, false).await
    }

    async fn custom_with_recovery(
        results: Vec<Result<DiscoveredCatalog, CatalogError>>,
        recovery: Option<DiscoveredCatalog>,
        public: bool,
    ) -> Self {
        const CUSTOM: &str = "custom-catalog-ownership";
        let profile = ProviderProfileV1 {
            catalog_selection_key: None,

            provider_id: CUSTOM.into(),
            display_name: "Fixture custom catalog".into(),
            api_family: ProviderApiFamilyWire::OpenAiChatCompletions,
            base_url: Some("http://127.0.0.1:32113".into()),
            response_open_timeout_ms: None,
            chunk_idle_timeout_ms: None,
            semantic_progress_timeout_ms: None,
            enabled: true,
            auth_requirement: if public {
                ProviderAuthRequirementWire::None
            } else {
                ProviderAuthRequirementWire::ApiKey
            },
            configured_models: vec![MODEL.into()],
            default_model: Some(MODEL.into()),
            promotion_model: None,
            provenance: crate::provider_registry::ProviderProvenance::Custom,
            trust: haider_rpc::ProviderTrustWire::Full,
        };
        let providers = ProviderRegistry::new(
            Box::new(TestProviderStore(StdMutex::new(vec![profile])))
                as Box<dyn ProviderRegistryStoreLike>,
            Vec::new(),
            Arc::new(CachedProviderModelSource::default()),
        )
        .expect("custom registry");
        Self::start(
            results,
            None,
            false,
            CUSTOM,
            AuthMethod::ApiKey,
            providers,
            recovery,
        )
        .await
    }

    async fn start(
        results: Vec<Result<DiscoveredCatalog, CatalogError>>,
        pin: Option<&str>,
        foreign: bool,
        provider: &'static str,
        auth_method: AuthMethod,
        providers: ProviderRegistry<Box<dyn ProviderRegistryStoreLike>>,
        pending_catalog: Option<DiscoveredCatalog>,
    ) -> Self {
        Self::start_with_replacement(
            results,
            pin,
            foreign,
            provider,
            auth_method,
            providers,
            pending_catalog,
            false,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_with_replacement(
        results: Vec<Result<DiscoveredCatalog, CatalogError>>,
        pin: Option<&str>,
        foreign: bool,
        provider: &'static str,
        auth_method: AuthMethod,
        mut providers: ProviderRegistry<Box<dyn ProviderRegistryStoreLike>>,
        pending_catalog: Option<DiscoveredCatalog>,
        replace_a: bool,
    ) -> Self {
        let dir = test_store_dir();
        let store = open_store(dir.path()).await;
        let mut accounts = memory_accounts();
        let vault = Arc::new(MemoryVault::new());
        for alias in ["a", "b"] {
            accounts
                .add(CredentialDescriptor {
                    incarnation: 0,
                    credential_digest: None,

                    alias: CredentialAlias::new(alias),
                    provider: provider.into(),
                    base_url: None,
                    auth_method,
                    identity: format!("fixture-{alias}"),
                    status: CredentialStatus::Ok,
                    active: alias == "b",
                    label: None,
                    account_identity: None,
                    created_at_ms: None,
                })
                .expect("account");
            let secret = if auth_method == AuthMethod::OAuth {
                refresh_oauth_bundle_with_access(alias.as_bytes())
                    .encode()
                    .expect("bundle")
            } else {
                zeroize::Zeroizing::new(alias.as_bytes().to_vec())
            };
            vault
                .put(&CredentialAlias::new(alias), &secret)
                .expect("fixture vault");
        }
        let retired_a = if replace_a {
            let old = accounts
                .get(&CredentialAlias::new("a"))
                .expect("original A")
                .clone();
            let mut replacement = old.clone();
            replacement.account_identity = Some(AccountIdentity {
                email: None,
                display_name: None,
                account_id: Some("fixture-replacement-account".into()),
                plan: None,
                issuer: Some("fixture-issuer".into()),
                captured_at: 1,
                verified: true,
            });
            accounts
                .replace(replacement)
                .expect("replace original account");
            Some(old)
        } else {
            None
        };
        if let Some(catalog) = pending_catalog {
            let input = ProviderConfigureInput {
                provider: provider.into(),
                api_family: None,
                origin: None,
                auth_requirement: None,
                enabled: true,
                models: vec![MODEL.into()],
                default_model: Some(MODEL.into()),
                response_open_timeout_ms: None,
                chunk_idle_timeout_ms: None,
                semantic_progress_timeout_ms: None,
                trust: None,
            };
            let (request_json, request_digest) = command_json(&ProviderConfigureIdentity {
                input: input.clone(),
                expected_revision: 0,
            })
            .expect("pending configure identity");
            let recovery = ProviderConfigureRecovery {
                input,
                previous_auth_requirement: None,
                previous_origin: providers
                    .get(provider)
                    .and_then(|profile| profile.base_url.clone()),
                catalog_producer: providers
                    .get(provider)
                    .filter(|profile| profile.auth_requirement == ProviderAuthRequirementWire::None)
                    .and_then(|profile| {
                        configure_catalog_producer(
                            "catalog-owner-test",
                            provider,
                            profile.base_url.as_deref(),
                            profile.auth_requirement,
                            &accounts,
                        )
                    }),
                discovered_models: Some(catalog.models),
                discovered_etag: catalog.etag,
                revision_unchanged: false,
                revision_unchanged_response: None,
            };
            assert!(matches!(
                store
                    .management_claim_receipt::<ProviderReceipt>(
                        "unowned-recovery".into(),
                        PROVIDER_CONFIGURE_METHOD.into(),
                        request_digest,
                        request_json,
                        Some(serde_json::to_string(&recovery).expect("pending recovery")),
                        Some(0),
                    )
                    .await
                    .expect("claim pending configure"),
                ManagementClaim::Fresh
            ));
            // The raw actor does not perform bootstrap reconciliation. Run
            // the production startup helper before publishing its registry.
            reconcile_provider_receipts(&store, "catalog-owner-test", &accounts, &mut providers)
                .await
                .expect("reconcile pending configure");
        }
        let snapshot = Arc::new(StdMutex::new(accounts.list().to_vec()));
        let management = ManagementSnapshot::new(
            store
                .management_revision()
                .await
                .expect("management revision"),
            accounts.list().to_vec(),
            providers.summaries(&|_| true),
        );
        let discoverer = Arc::new(BlockingModelDiscoverer {
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            seen: StdMutex::new(Vec::new()),
            results: StdMutex::new(
                results
                    .into_iter()
                    .map(ModelDiscoveryFixture::Return)
                    .collect(),
            ),
        });
        let broker_vault = vault.clone() as Arc<dyn Vault>;
        let broker_snapshot = snapshot.clone();
        let (actor, _) = start_account_actor_with_services(
            AccountActorConfig {
                store: store.clone(),
                accounts,
                vault: vault.clone(),
                validator: Arc::new(ProviderCredentialValidator),
                snapshot: snapshot.clone(),
                management: Some(management.clone()),
                device_discovery: DeviceDiscoverySnapshot::new(true),
                profile_id: "catalog-owner-test".into(),
                default_model: "unused".into(),
                providers,
                provider_endpoint_validator: Arc::new(ProductionProviderEndpointValidator),
                reserved_aliases: HashSet::new(),
                refresh_fences: RefreshFenceRegistry::default(),
                source_registry: empty_source_registry(),
                source_snapshot: empty_source_snapshot(),
            },
            |commands| {
                CredentialBroker::new(
                    broker_vault,
                    refresh_oauth_catalog(),
                    broker_snapshot,
                    commands,
                )
            },
            discoverer.clone(),
            Arc::new(UnreachableGcloud),
            Arc::new(PlatformClaudeNativeCredentialStore::default()),
        )
        .expect("actor");
        let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
        hub.install_accounts(AccountsFacade {
            login: Some(actor.commands()),
            oauth: None,
            snapshot,
            management,
            vault_supported: true,
            discovery_disabled: true,
            device_discovery: DeviceDiscoverySnapshot::new(true),
            sources: empty_source_snapshot(),
            vault: Some(vault),
        })
        .expect("accounts facade");
        hub.install_creatable_providers(std::collections::BTreeSet::from([
            provider.into(),
            OPENAI_PROVIDER_NAME.into(),
        ]))
        .expect("creatable");
        let session = SessionId::new("catalog-owner-session");
        hub.create_internal_session(haider_core::SessionCreateCommand {
            resolved_model: None,
            model_catalog_key: None,

            command_id: "initial-create".into(),
            request_digest: "initial-digest".into(),
            request_json: "{}".into(),
            session_id: session.clone(),
            cwd: dir.path().to_string_lossy().into_owned(),
            provider: if foreign {
                OPENAI_PROVIDER_NAME
            } else {
                provider
            }
            .into(),
            model: INITIAL_MODEL.into(),
            max_tokens: 4096,
            max_tokens_source: Some(
                haider_protocol::output_budget::SessionOutputBudgetSourceV1::UserSet {
                    requested: 4096,
                },
            ),
            permission_overrides: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: "fixture-v1".into(),
            event_id: EventId::new("initial-create"),
            device_id: DeviceId::new("fixture"),
        })
        .await
        .expect("initial session");
        let (sink, frames) = channel_sink();
        let connection = hub
            .open_connection(
                std::collections::BTreeSet::from([Capability::View, Capability::Control]),
                sink,
                ConnectionTransport::LocalSameUid,
            )
            .expect("connection");
        connection
            .request(
                RequestId::new("attach"),
                RequestBody::SessionAttach {
                    session_id: session.clone(),
                    after_seq: 0,
                    mode: AttachMode::Control,
                    sealed_replay: false,
                },
            )
            .await
            .expect("attach");
        let fixture = Self {
            dir,
            provider,
            store,
            actor,
            hub,
            connection,
            frames,
            discoverer,
            session,
            retired_a,
        };
        if let Some(pin) = pin {
            fixture.pin(pin, "initial-pin").await;
        }
        fixture
    }

    async fn pin(&self, account: &str, id: &str) {
        self.hub
            .rebind_session_provider(haider_store::SessionProviderRebindCommand {
                command_id: id.into(),
                request_digest: id.into(),
                request_json: format!("{{\"fixture\":\"{id}\"}}"),
                session_id: self.session.clone(),
                worker_generation: self.store.worker_generation(),
                provider: self.provider.into(),
                base_url: None,
                account: Some(account.into()),
                max_tokens: 4096,
                max_tokens_source:
                    haider_protocol::output_budget::SessionOutputBudgetSourceV1::UserSet {
                        requested: 4096,
                    },
                clamped: None,
                active_output_budget: None,
                event_id: EventId::new(id),
                device_id: DeviceId::new("fixture"),
            })
            .await
            .expect("rebind");
    }

    async fn begin_refresh(
        &self,
    ) -> tokio::sync::oneshot::Receiver<Result<ProviderSummaryWire, ProviderModelsRefreshFailure>>
    {
        self.begin_refresh_for("a").await
    }

    async fn begin_refresh_for(
        &self,
        alias: &str,
    ) -> tokio::sync::oneshot::Receiver<Result<ProviderSummaryWire, ProviderModelsRefreshFailure>>
    {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        self.actor
            .commands()
            .send(AccountCommand::RefreshProviderModels {
                provider: self.provider.into(),
                account_alias: Some(CredentialAlias::new(alias)),
                completed: ProviderModelsRefreshCompletion::internal(sender),
            })
            .await
            .expect("refresh A");
        tokio::time::timeout(
            haider_platform::TEST_WAIT_TIMEOUT,
            self.discoverer.started.notified(),
        )
        .await
        .expect("A started");
        receiver
    }

    async fn publish_a(&self) {
        let receiver = self.begin_refresh().await;
        self.discoverer.release.notify_one();
        receiver.await.expect("A completion").expect("A catalog");
    }

    fn selection(&self, foreign: bool, budget: Option<u64>) -> RequestBody {
        RequestBody::SessionSelectModel {
            expected_catalog_key: None,
            command_id: CommandId::new("select-owned"),
            session_id: self.session.clone(),
            worker_generation: self.store.worker_generation(),
            provider: foreign.then(|| self.provider.into()),
            model: MODEL.into(),
            confirm_new_epoch: true,
            max_tokens: budget,
        }
    }

    fn creation(&self, pinned: bool, budget: u64) -> RequestBody {
        RequestBody::SessionCreateWithPermissionOverrides {
            expected_catalog_key: None,
            command_id: CommandId::new("create-owned"),
            cwd: self.dir.path().to_string_lossy().into_owned(),
            provider: self.provider.into(),
            model: MODEL.into(),
            max_tokens: budget,
            permission_overrides: None,
            workspace_allocation: None,
            cache_policy: None,
            interaction_mode: haider_protocol::session::SessionInteractionModeV1::Autonomous,
            ssh_scope: None,
            account_alias: pinned.then(|| CredentialAlias::new("b")),
            resolve_provider: false,
            resolve_model: false,
            effort: None,
            fast: None,
        }
    }

    async fn race_selection(&mut self) -> ResponseBody {
        let selection = self
            .connection
            .request(RequestId::new("admit"), self.selection(false, None));
        tokio::pin!(selection);
        tokio::select! {
            _ = self.discoverer.started.notified() => {},
            result = &mut selection => panic!("selection finished before A discovery: {result:?}"),
        }
        self.connection
            .request(
                RequestId::new("rebind-during-refresh"),
                RequestBody::SessionProviderRebind {
                    command_id: CommandId::new("concurrent-rebind"),
                    session_id: self.session.clone(),
                    worker_generation: self.store.worker_generation(),
                    provider: self.provider.into(),
                    base_url: None,
                    account: Some("b".into()),
                },
            )
            .await
            .expect("concurrent rebind RPC");
        let rebound = response_for(&mut self.frames, "rebind-during-refresh").await;
        assert!(
            !matches!(rebound, ResponseBody::Error { .. }),
            "the real rebind RPC must commit while A is paused: {rebound:?}"
        );
        assert_eq!(
            self.hub
                .session_metadata(&self.session)
                .await
                .expect("rebind readback")
                .expect("session")
                .account_alias
                .as_deref(),
            Some("b")
        );
        self.discoverer.release.notify_one();
        tokio::select! {
            _ = self.discoverer.started.notified() => {},
            result = &mut selection => panic!("selection used A publication after B rebind: {result:?}"),
        }
        assert_eq!(
            self.discoverer
                .seen
                .lock()
                .expect("observations")
                .last()
                .expect("B call")
                .1
                .as_deref(),
            Some("b")
        );
        self.discoverer.release.notify_one();
        selection.await.expect("selection");
        response_for(&mut self.frames, "admit").await
    }

    async fn admit(&mut self, request: RequestBody) -> ResponseBody {
        let selection = self.connection.request(RequestId::new("admit"), request);
        tokio::pin!(selection);
        tokio::select! {
            _ = self.discoverer.started.notified() => {},
            result = &mut selection => panic!("admission used another account's catalog without B discovery: {result:?}"),
        }
        assert_eq!(
            self.discoverer
                .seen
                .lock()
                .expect("observations")
                .last()
                .expect("B call")
                .1
                .as_deref(),
            Some("b"),
            "discovery must use B's credential"
        );
        self.discoverer.release.notify_one();
        selection.await.expect("admission");
        response_for(&mut self.frames, "admit").await
    }

    async fn close(mut self) {
        self.connection.close().await.expect("connection close");
        self.hub.shutdown().await.expect("hub shutdown");
        self.actor.shutdown().await;
        self.store.close().await.expect("store close");
    }
}

async fn rejects_a_only_model(
    creation: Option<bool>,
    pin: Option<&str>,
    foreign: bool,
    cached: bool,
    overlap: bool,
) {
    let mut results = Vec::new();
    if overlap {
        // A maintained Static inventory refuses an unknown slug without
        // discovery. Establish the real fetched A catalog first, just as the
        // live sign-in journey does, then pause its refresh-on-miss below.
        results.push(Ok(other_catalog()));
    }
    results.push(Ok(catalog(2048)));
    if cached {
        results.push(Err(CatalogError::NotModified));
    }
    results.push(Ok(other_catalog()));
    let mut f = Fixture::new(results, pin, foreign).await;
    if cached || overlap {
        f.publish_a().await;
    }
    let response = if overlap {
        f.race_selection().await
    } else {
        f.publish_a().await;
        let request = creation.map_or_else(
            || f.selection(foreign, None),
            |pinned| f.creation(pinned, 1024),
        );
        f.admit(request).await
    };
    assert!(
        matches!(response, ResponseBody::Error { ref code, .. } if code == haider_rpc::ERROR_CODE_MODEL_UNKNOWN),
        "B cannot admit A's model: {response:?}"
    );
    f.close().await;
}

#[tokio::test]
async fn catalog_owner_paused_a_rebind_b() {
    rejects_a_only_model(None, Some("a"), false, false, true).await;
}
#[tokio::test]
async fn catalog_owner_later_b_pinned_creation() {
    rejects_a_only_model(Some(true), None, false, false, false).await;
}
#[tokio::test]
async fn catalog_owner_sequential_b_pinned_selection() {
    rejects_a_only_model(None, Some("b"), false, false, false).await;
}
#[tokio::test]
async fn catalog_owner_304_republication() {
    rejects_a_only_model(None, Some("b"), false, true, false).await;
}
#[tokio::test]
async fn catalog_owner_explicit_foreign_provider() {
    rejects_a_only_model(None, None, true, false, false).await;
}
#[tokio::test]
async fn catalog_owner_default_b_creation() {
    rejects_a_only_model(Some(false), None, false, false, false).await;
}
#[tokio::test]
async fn catalog_owner_default_b_selection() {
    rejects_a_only_model(None, None, false, false, false).await;
}

async fn metadata_is_b(creation: bool, a: u64, b: u64, explicit: bool) {
    let mut f = Fixture::new(vec![Ok(catalog(a)), Ok(catalog(b))], Some("b"), false).await;
    f.publish_a().await;
    let request = if creation {
        f.creation(true, 4096)
    } else {
        f.selection(false, explicit.then_some(4096))
    };
    let response = f.admit(request).await;
    if explicit && b < 4096 {
        assert!(
            matches!(response, ResponseBody::Error { ref code, .. } if code == haider_rpc::ERROR_CODE_INVALID_ARGUMENT),
            "B limit must refuse explicit budget: {response:?}"
        );
    } else if creation {
        let ResponseBody::SessionCreate { metadata, .. } = response else {
            panic!("creation must use B's metadata: {response:?}");
        };
        assert_eq!(metadata.max_tokens, 4096);
    } else {
        assert!(
            !matches!(response, ResponseBody::Error { .. }),
            "selection must use B's metadata: {response:?}"
        );
        let metadata = f
            .hub
            .session_metadata(&f.session)
            .await
            .expect("read")
            .expect("session");
        assert_eq!(
            metadata.max_tokens,
            if explicit { 4096 } else { b.min(4096) },
            "stored user budget clamps against B only"
        );
    }
    f.close().await;
}
#[tokio::test]
async fn catalog_owner_metadata_explicit_selection() {
    metadata_is_b(false, 2048, 8192, true).await;
}
#[tokio::test]
async fn catalog_owner_metadata_explicit_creation() {
    metadata_is_b(true, 2048, 8192, true).await;
}
#[tokio::test]
async fn catalog_owner_metadata_rejects_b_overflow() {
    metadata_is_b(false, 8192, 2048, true).await;
}
#[tokio::test]
async fn catalog_owner_metadata_stored_budget_preserved() {
    metadata_is_b(false, 2048, 8192, false).await;
}
#[tokio::test]
async fn catalog_owner_metadata_stored_budget_clamped() {
    metadata_is_b(false, 8192, 2048, false).await;
}

async fn removed_refresh_is_discarded(cached: bool) {
    let results = if cached {
        vec![Ok(catalog(2048)), Err(CatalogError::NotModified)]
    } else {
        vec![Ok(catalog(2048))]
    };
    let f = Fixture::new(results, None, false).await;
    if cached {
        f.publish_a().await;
    }
    let receiver = f.begin_refresh().await;
    let (sink, mut frames) = channel_sink();
    f.actor
        .commands()
        .send(AccountCommand::Remove(Box::new(RemoveAccountJob {
            command_id: "remove-inactive-a".into(),
            alias: "a".into(),
            expected_revision: None,
            route: LoginRoute {
                request_id: RequestId::new("remove-a"),
                sink,
            },
        })))
        .await
        .expect("remove A");
    let response = frames.recv().await.expect("remove response");
    assert!(
        !matches!(
            response,
            WireFrame::Response {
                body: ResponseBody::Error { .. },
                ..
            }
        ),
        "remove must commit during flight: {response:?}"
    );
    f.discoverer.release.notify_one();
    let error = receiver
        .await
        .expect("completion")
        .expect_err("removed catalog owner cannot publish");
    assert_eq!(error.code, haider_rpc::ERROR_CODE_BUSY);
    let view = f
        .hub
        .accounts()
        .expect("facade")
        .expect("accounts")
        .management
        .read()
        .expect("view");
    assert!(view.descriptors.iter().all(|row| row.alias.as_str() != "a"));
    f.close().await;
}
#[tokio::test]
async fn catalog_owner_inactive_removed_success() {
    removed_refresh_is_discarded(false).await;
}
#[tokio::test]
async fn catalog_owner_inactive_removed_304() {
    removed_refresh_is_discarded(true).await;
}

// Reproduce an already-queued completion after the alias has been replaced.
// This uses the actual actor's completion arm (and real SQLite publication),
// while injecting the stale response at its asynchronous delivery boundary.
async fn replaced_refresh_is_discarded(cached: bool) {
    let f = Fixture::start_with_replacement(
        Vec::new(),
        None,
        false,
        PROVIDER,
        AuthMethod::OAuth,
        test_provider_registry(),
        None,
        true,
    )
    .await;
    let view = f
        .hub
        .accounts()
        .expect("facade")
        .expect("accounts")
        .management
        .read()
        .expect("view");
    let current = view
        .descriptors
        .iter()
        .find(|row| row.alias.as_str() == "a")
        .expect("A")
        .clone();
    let old = f.retired_a.clone().expect("captured original A");
    assert_ne!(
        ProviderModelCacheKey::for_account(PROVIDER, &old),
        ProviderModelCacheKey::for_account(PROVIDER, &current),
        "the real account replacement must retire the captured producer"
    );
    let active = view.descriptors.iter().find(|row| row.active).expect("B");
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let models = catalog(2048);
    f.actor
        .commands()
        .send(AccountCommand::ProviderModelsRefreshCompleted {
            provider: PROVIDER.into(),
            producer: CatalogProducer {
                profile: "catalog-owner-test".into(),
                provider: PROVIDER.into(),
                source: CatalogSource::OpenAiSubscription,
                origin: view
                    .providers
                    .iter()
                    .find(|row| row.provider == PROVIDER)
                    .and_then(|row| row.endpoint.clone()),
                auth: ProviderAuthRequirementWire::OAuth,
                alias: Some(old.alias.clone()),
                incarnation: Some(old.incarnation),
                cache_key: ProviderModelCacheKey::for_account(PROVIDER, &old),
            },
            account_alias: Some(old.alias.clone()),
            explicit_account: true,
            cache_key: ProviderModelCacheKey::for_account(PROVIDER, &old),
            expected_active_cache_key: Some(ProviderModelCacheKey::for_account(PROVIDER, active)),
            cached: cached.then(|| haider_core::CachedModels {
                models_json: serde_json::to_string(&models.models).expect("models"),
                etag: models.etag.clone(),
                fetched_at_ms: 1,
            }),
            result: ProviderModelsRefreshResult::Discovery(if cached {
                Err(CatalogError::NotModified)
            } else {
                Ok(models)
            }),
            completed: ProviderModelsRefreshCompletion::internal(sender),
        })
        .await
        .expect("stale completion");
    let error = receiver
        .await
        .expect("completion")
        .expect_err("replaced catalog owner cannot publish");
    assert_eq!(error.code, haider_rpc::ERROR_CODE_BUSY);
    f.close().await;
}
#[tokio::test]
async fn catalog_owner_inactive_replaced_success() {
    replaced_refresh_is_discarded(false).await;
}
#[tokio::test]
async fn catalog_owner_inactive_replaced_304() {
    replaced_refresh_is_discarded(true).await;
}

async fn context_window_is_b(a_window: u64, b_window: u64) {
    let mut a = catalog(8192);
    a.models[0].context_window = Some(a_window);
    let mut b = catalog(8192);
    b.models[0].context_window = Some(b_window);
    let mut f = Fixture::new(vec![Ok(a), Ok(b)], Some("b"), false).await;
    f.publish_a().await;
    let request = f.selection(false, Some(4096));
    let response = f.admit(request).await;
    if b_window == 4096 {
        assert!(
            matches!(
                response,
                ResponseBody::Error {
                    data: Some(haider_rpc::ErrorData::ModelOutputLimit {
                        context_window: Some(4096),
                        max_output_tokens: 2048,
                        ..
                    }),
                    ..
                }
            ),
            "B context must bound its output: {response:?}"
        );
    } else {
        assert!(
            !matches!(response, ResponseBody::Error { .. }),
            "A context must not bound B output: {response:?}"
        );
    }
    f.close().await;
}
#[tokio::test]
async fn catalog_owner_metadata_context_window_refuses() {
    context_window_is_b(32768, 4096).await;
}
#[tokio::test]
async fn catalog_owner_metadata_context_window_accepts() {
    context_window_is_b(4096, 32768).await;
}

#[tokio::test]
async fn catalog_owner_saved_model_cannot_reuse_another_accounts_admission() {
    let f = Fixture::new(vec![Ok(catalog(8192))], Some("b"), false).await;
    f.publish_a().await;
    let facade = f.hub.accounts().expect("facade").expect("accounts");
    let factory = AccountsProviderFactory::new_with_management(
        facade.snapshot.clone(),
        facade.management.clone(),
        VaultProvision::Unsupported,
        Arc::new(ProductionAccountBuilder::default()),
    );
    let mut metadata = f
        .hub
        .session_metadata(&f.session)
        .await
        .expect("read")
        .expect("metadata");
    metadata.model = MODEL.into();
    assert!(
        factory.admit_persisted_model(&metadata).is_err(),
        "A's model list must not authorize B's saved model"
    );
    f.close().await;
}

async fn owning_account_cache_hit(cached: bool) {
    let results = if cached {
        vec![Ok(catalog(8192)), Err(CatalogError::NotModified)]
    } else {
        vec![Ok(catalog(8192))]
    };
    let mut f = Fixture::new(results, Some("a"), false).await;
    f.publish_a().await;
    if cached {
        f.publish_a().await;
    }
    f.connection
        .request(RequestId::new("own-cache"), f.selection(false, Some(4096)))
        .await
        .expect("own cached selection");
    let body = response_for(&mut f.frames, "own-cache").await;
    assert!(
        !matches!(body, ResponseBody::Error { .. }),
        "a correctly owned catalog must remain usable: {body:?}"
    );
    assert_eq!(
        f.discoverer.seen.lock().expect("calls").len(),
        if cached { 2 } else { 1 },
        "own fresh catalog must skip discovery"
    );
    let metadata = f
        .hub
        .session_metadata(&f.session)
        .await
        .expect("read")
        .expect("session");
    assert_eq!(metadata.account_alias.as_deref(), Some("a"));
    assert_eq!(metadata.model, MODEL);
    f.close().await;
}
#[tokio::test]
async fn catalog_control_owned_success_cache_hit() {
    owning_account_cache_hit(false).await;
}
#[tokio::test]
async fn catalog_control_owned_304_cache_hit() {
    owning_account_cache_hit(true).await;
}

async fn child_catalog_is_scoped(explicit_provider: bool) {
    let f = Fixture::new(
        vec![Ok(catalog(8192))],
        Some(if explicit_provider { "a" } else { "b" }),
        false,
    )
    .await;
    f.publish_a().await;
    let parent = f
        .hub
        .session_metadata(&f.session)
        .await
        .expect("parent")
        .expect("metadata");
    let delegation = crate::delegation::DelegationHandle::new(f.hub.clone());
    let mut arguments = serde_json::json!({
        "task":"synthetic child selection", "prompt":"fixture only", "model": MODEL,
    });
    if explicit_provider {
        arguments["provider"] = serde_json::json!(PROVIDER);
    }
    let request = haider_tools::SpawnSubagent::from_tool_args(arguments).expect("child request");
    let selected = delegation
        .resolve_child_metadata(&parent, &request)
        .expect("resolution");
    assert!(
        selected.is_err(),
        "child selection must not use A catalog for B: {selected:?}"
    );
    f.close().await;
}
#[tokio::test]
async fn catalog_owner_child_inherits_b_pin() {
    child_catalog_is_scoped(false).await;
}
#[tokio::test]
async fn catalog_owner_child_explicit_provider_uses_default_b() {
    child_catalog_is_scoped(true).await;
}

#[tokio::test]
async fn catalog_owner_rebind_budget_ignores_other_accounts_remote_override() {
    let mut a = catalog(2048);
    a.models[0].slug = INITIAL_MODEL.into();
    let mut f = Fixture::new(vec![Ok(a)], Some("a"), false).await;
    f.publish_a().await;
    f.connection
        .request(
            RequestId::new("rebind-owned"),
            RequestBody::SessionProviderRebind {
                command_id: CommandId::new("rebind-owned"),
                session_id: f.session.clone(),
                worker_generation: f.store.worker_generation(),
                provider: PROVIDER.into(),
                base_url: None,
                account: Some("b".into()),
            },
        )
        .await
        .expect("rebind request");
    let body = response_for(&mut f.frames, "rebind-owned").await;
    assert!(
        !matches!(body, ResponseBody::Error { .. }),
        "maintained model remains usable: {body:?}"
    );
    let metadata = f
        .hub
        .session_metadata(&f.session)
        .await
        .expect("read")
        .expect("metadata");
    assert_eq!(metadata.account_alias.as_deref(), Some("b"));
    assert_eq!(
        metadata.max_tokens, 4096,
        "A's remote 2048 maximum must not clamp B"
    );
    f.close().await;
}

async fn default_model_comes_from_b(pinned: bool) {
    let mut a = catalog(2048);
    a.models[0].priority = Some(-1);
    let mut f = Fixture::new(vec![Ok(a), Ok(other_catalog())], None, false).await;
    f.publish_a().await;
    let mut request = f.creation(pinned, 0);
    if let RequestBody::SessionCreateWithPermissionOverrides {
        model,
        resolve_model,
        ..
    } = &mut request
    {
        model.clear();
        *resolve_model = true;
    }
    let response = f.admit(request).await;
    let ResponseBody::SessionCreate { metadata, .. } = response else {
        panic!("B's own maintained default must create: {response:?}");
    };
    assert_ne!(
        metadata.model, MODEL,
        "B's omitted model cannot resolve to A's default"
    );
    assert_eq!(metadata.account_alias.as_deref(), pinned.then_some("b"));
    f.close().await;
}
#[tokio::test]
async fn catalog_owner_pinned_creation_resolves_b_default() {
    default_model_comes_from_b(true).await;
}
#[tokio::test]
async fn catalog_owner_unpinned_creation_resolves_b_default() {
    default_model_comes_from_b(false).await;
}

async fn maintained_default_fixture() -> (Fixture, String, String) {
    let baseline = test_provider_registry()
        .summaries(&|_| true)
        .into_iter()
        .find(|summary| summary.provider == PROVIDER)
        .expect("maintained provider");
    let default = baseline.default_model.expect("maintained default");
    let alternate = baseline
        .models
        .into_iter()
        .find(|model| model != &default)
        .expect("another maintained model");
    let mut a = catalog(8192);
    a.models[0].slug = alternate.clone();
    a.models[0].display_name = alternate.clone();
    a.models[0].priority = Some(-1000);
    let f = Fixture::new(vec![Ok(a)], None, false).await;
    f.publish_a().await;
    let view = f
        .hub
        .accounts()
        .expect("facade")
        .expect("accounts")
        .management
        .read()
        .expect("view");
    assert_eq!(
        view.providers
            .iter()
            .find(|summary| summary.provider == PROVIDER)
            .expect("A summary")
            .default_model
            .as_deref(),
        Some(alternate.as_str()),
        "real A publication must establish its account-specific ordering"
    );
    (f, default, alternate)
}

#[tokio::test]
async fn catalog_owner_maintained_slug_cannot_keep_another_accounts_default() {
    let (f, default, _) = maintained_default_fixture().await;
    let facade = f.hub.accounts().expect("facade").expect("accounts");
    let factory = AccountsProviderFactory::new_with_management(
        facade.snapshot.clone(),
        facade.management.clone(),
        VaultProvision::Unsupported,
        Arc::new(ProductionAccountBuilder::default()),
    );
    assert_eq!(
        factory
            .provider_profile(PROVIDER)
            .expect("default B profile")
            .default_model
            .as_deref(),
        Some(default.as_str()),
        "B must not inherit A's default merely because its slug is also maintained"
    );
    f.close().await;
}

#[tokio::test]
async fn catalog_control_owned_maintained_slug_default_is_preserved() {
    let (mut f, _, alternate) = maintained_default_fixture().await;
    let mut request = f.creation(true, 4096);
    if let RequestBody::SessionCreateWithPermissionOverrides {
        account_alias,
        model,
        resolve_model,
        ..
    } = &mut request
    {
        *account_alias = Some(CredentialAlias::new("a"));
        model.clear();
        *resolve_model = true;
    }
    f.connection
        .request(RequestId::new("owned-default"), request)
        .await
        .expect("owned default creation");
    let response = response_for(&mut f.frames, "owned-default").await;
    let ResponseBody::SessionCreate { metadata, .. } = response else {
        panic!("A's owned default must remain usable: {response:?}");
    };
    assert_eq!(metadata.model, alternate);
    assert_eq!(metadata.account_alias.as_deref(), Some("a"));
    assert_eq!(f.discoverer.seen.lock().expect("calls").len(), 1);
    f.close().await;
}

async fn hidden_catalog_fixture(active_a: bool) -> Fixture {
    let mut hidden = catalog(8192);
    hidden.models = haider_provider::subscription_static_models(PROVIDER)
        .into_iter()
        .map(|mut model| {
            model.visible = false;
            model
        })
        .collect();
    assert!(!hidden.models.is_empty(), "maintained fixture models");
    let f = Fixture::new(vec![Ok(hidden)], None, false).await;
    if active_a {
        let (sink, mut frames) = channel_sink();
        f.actor
            .commands()
            .send(AccountCommand::SetActive(Box::new(SetActiveJob {
                command_id: "hidden-catalog-active-a".into(),
                alias: "a".into(),
                route: LoginRoute {
                    request_id: RequestId::new("hidden-catalog-active-a"),
                    sink,
                },
            })))
            .await
            .expect("activate A before its discovery");
        assert!(matches!(
            frames.recv().await.expect("active response"),
            WireFrame::Response {
                body: ResponseBody::AccountSetActive { .. },
                ..
            }
        ));
    }
    f.publish_a().await;
    let view = f
        .hub
        .accounts()
        .expect("facade")
        .expect("accounts")
        .management
        .read()
        .expect("A publication");
    let a = view
        .providers
        .iter()
        .find(|summary| summary.provider == PROVIDER)
        .expect("A summary");
    assert!(a.models.is_empty(), "A's hidden rows must be published");
    assert_eq!(a.availability, ProviderAvailabilityWire::Unavailable);
    f
}

async fn hidden_catalog_does_not_change_other_account_availability(owned: bool) {
    let f = hidden_catalog_fixture(owned).await;
    let facade = f.hub.accounts().expect("facade").expect("accounts");
    let factory = AccountsProviderFactory::new_with_management(
        facade.snapshot.clone(),
        facade.management.clone(),
        VaultProvision::Unsupported,
        Arc::new(ProductionAccountBuilder::default()),
    );
    let profile = factory.provider_profile(PROVIDER).expect("default profile");
    assert_eq!(
        profile.availability,
        if owned {
            ProviderAvailabilityWire::Unavailable
        } else {
            ProviderAvailabilityWire::Available
        },
        "availability must belong to the account whose profile is consumed"
    );
    assert_eq!(profile.models.is_empty(), owned);
    assert_eq!(f.discoverer.seen.lock().expect("calls").len(), 1);
    f.close().await;
}

#[tokio::test]
async fn catalog_owner_hidden_a_rows_cannot_disable_b_maintained_inventory() {
    hidden_catalog_does_not_change_other_account_availability(false).await;
}

#[tokio::test]
async fn catalog_control_owned_hidden_rows_remain_unavailable() {
    hidden_catalog_does_not_change_other_account_availability(true).await;
}

#[tokio::test]
async fn catalog_owner_removed_alias_cannot_serve_a_stale_cached_model() {
    let mut f = Fixture::new(vec![Ok(catalog(2048)), Ok(catalog(2048))], Some("a"), false).await;
    f.publish_a().await;
    // Advance only freshness of the actor's actually published catalog. Its
    // rows and owning account still come from the real successful completion.
    let facade = f.hub.accounts().expect("facade").expect("accounts");
    facade
        .management
        .inner
        .lock()
        .expect("view")
        .providers
        .iter_mut()
        .find(|row| row.provider == PROVIDER)
        .expect("summary")
        .inventory = haider_rpc::ModelInventoryWire::Stale {
        fetched_at_ms: 1,
        reason: "fixture TTL elapsed".into(),
    };
    {
        let selection = f.connection.request(
            RequestId::new("removed-owner-selection"),
            f.selection(false, None),
        );
        tokio::pin!(selection);
        tokio::select! {
            _ = f.discoverer.started.notified() => {},
            result = &mut selection => panic!("stale catalog must refresh first: {result:?}"),
        }
        let (sink, mut frames) = channel_sink();
        f.actor
            .commands()
            .send(AccountCommand::Remove(Box::new(RemoveAccountJob {
                command_id: "remove-cached-owner".into(),
                alias: "a".into(),
                expected_revision: None,
                route: LoginRoute {
                    request_id: RequestId::new("remove-cached-owner"),
                    sink,
                },
            })))
            .await
            .expect("remove owner");
        let removed = frames.recv().await.expect("remove result");
        assert!(
            !matches!(
                removed,
                WireFrame::Response {
                    body: ResponseBody::Error { .. },
                    ..
                }
            ),
            "remove must commit: {removed:?}"
        );
        f.discoverer.release.notify_one();
        selection.await.expect("selection response");
        let body = response_for(&mut f.frames, "removed-owner-selection").await;
        assert!(
            matches!(body, ResponseBody::Error { .. }),
            "an invalidated owner cannot authorize cached admission: {body:?}"
        );
    }
    let metadata = f
        .hub
        .session_metadata(&f.session)
        .await
        .expect("read")
        .expect("metadata");
    assert_eq!(metadata.model, INITIAL_MODEL);
    f.close().await;
}

async fn custom_default_ignores_discovery_failure(other_account_catalog: bool) {
    let mut first = catalog(2048);
    if !other_account_catalog {
        first.models[0].slug = "listed-other-model".into();
    }
    let mut f = Fixture::custom(vec![
        Ok(first),
        Err(CatalogError::Unavailable {
            reason: "synthetic advisory outage".into(),
        }),
    ])
    .await;
    let receiver = f
        .begin_refresh_for(if other_account_catalog { "a" } else { "b" })
        .await;
    f.discoverer.release.notify_one();
    receiver.await.expect("completion").expect("catalog");
    let mut request = f.creation(true, 4096);
    if let RequestBody::SessionCreateWithPermissionOverrides {
        model,
        resolve_model,
        ..
    } = &mut request
    {
        model.clear();
        *resolve_model = true;
    }
    let response = f.admit(request).await;
    let ResponseBody::SessionCreate { metadata, .. } = response else {
        panic!("configured advisory default must survive discovery failure: {response:?}");
    };
    assert_eq!(metadata.model, MODEL);
    assert_eq!(metadata.account_alias.as_deref(), Some("b"));
    assert_eq!(
        metadata.max_tokens, 4096,
        "A's declared 2048 maximum must not clamp custom B"
    );
    assert_eq!(
        f.discoverer.seen.lock().expect("calls").len(),
        2,
        "one initial catalog and exactly one B admission probe"
    );
    f.close().await;
}
#[tokio::test]
async fn catalog_owner_custom_configured_default_ignores_a_limits() {
    custom_default_ignores_discovery_failure(true).await;
}
#[tokio::test]
async fn catalog_control_advisory_default_probes_once() {
    custom_default_ignores_discovery_failure(false).await;
}

#[tokio::test]
async fn catalog_owner_resolved_retry_cannot_admit_using_session_default() {
    resolved_retry_uses_its_own_catalog(false).await;
}

#[tokio::test]
async fn catalog_owner_stale_resolved_descriptor_cannot_use_replacement_catalog() {
    resolved_retry_uses_its_own_catalog(true).await;
}

async fn resolved_retry_uses_its_own_catalog(replaced: bool) {
    let f = Fixture::new(vec![Ok(catalog(8192))], Some("a"), false).await;
    f.publish_a().await;
    let facade = f.hub.accounts().expect("facade").expect("accounts");
    let mut descriptor = facade
        .snapshot
        .lock()
        .expect("accounts")
        .iter()
        .find(|row| row.alias.as_str() == if replaced { "a" } else { "b" })
        .expect("resolved account")
        .clone();
    if replaced {
        descriptor.identity = "previous-incarnation".into();
        descriptor.account_identity = None;
    }
    let factory = AccountsProviderFactory::new_with_management(
        facade.snapshot.clone(),
        facade.management.clone(),
        VaultProvision::Unsupported,
        TuningRecordingBuilder::new(),
    );
    let mut metadata = f
        .hub
        .session_metadata(&f.session)
        .await
        .expect("read")
        .expect("session");
    metadata.model = MODEL.into();
    let vault = MemoryVault::default();
    vault
        .put(&descriptor.alias, b"fixture-only")
        .expect("fake key");
    let result = factory.build_provider(
        &descriptor,
        vault.resolve(&descriptor.alias).expect("fake credential"),
        &metadata,
        &ProviderTuning::default(),
    );
    assert!(
        result.is_err(),
        "a resolved retry cannot use another account identity's catalog"
    );
    assert_eq!(
        metadata.account_alias.as_deref(),
        Some("a"),
        "admission must not rewrite the durable pin"
    );
    f.close().await;
}

#[tokio::test]
async fn catalog_control_active_identity_change_rejects_inflight_a() {
    let f = Fixture::new(vec![Ok(catalog(8192))], None, false).await;
    let receiver = f.begin_refresh().await;
    let (sink, mut frames) = channel_sink();
    f.actor
        .commands()
        .send(AccountCommand::SetActive(Box::new(SetActiveJob {
            command_id: "change-active-during-flight".into(),
            alias: "a".into(),
            route: LoginRoute {
                request_id: RequestId::new("change-active"),
                sink,
            },
        })))
        .await
        .expect("active switch");
    let response = frames.recv().await.expect("switch response");
    assert!(
        matches!(
            response,
            WireFrame::Response {
                body: ResponseBody::AccountSetActive { .. },
                ..
            }
        ),
        "active switch must commit during discovery: {response:?}"
    );
    f.discoverer.release.notify_one();
    let failure = receiver
        .await
        .expect("completion")
        .expect_err("active identity change must reject stale completion");
    assert_eq!(failure.code, haider_rpc::ERROR_CODE_BUSY);
    assert!(failure.retryable);
    let view = f
        .hub
        .accounts()
        .expect("facade")
        .expect("accounts")
        .management
        .read()
        .expect("view");
    assert!(
        !view
            .providers
            .iter()
            .find(|row| row.provider == PROVIDER)
            .expect("provider")
            .models
            .iter()
            .any(|model| model == MODEL),
        "rejected completion must not publish A rows"
    );
    f.close().await;
}

#[tokio::test]
async fn catalog_owner_adapter_cache_uses_canonical_account_identity() {
    let f = Fixture::new(vec![Ok(catalog(8192))], Some("a"), false).await;
    f.publish_a().await;
    let view = f
        .hub
        .accounts()
        .expect("facade")
        .expect("accounts")
        .management
        .read()
        .expect("view");
    let descriptor = view
        .descriptors
        .iter()
        .find(|row| row.alias.as_str() == "a")
        .expect("A");
    let mut account_store = memory_accounts();
    account_store
        .add(descriptor.clone())
        .expect("persist original account");
    let descriptor = account_store
        .get(&descriptor.alias)
        .expect("stored original")
        .clone();
    let mut other_identity = descriptor.clone();
    other_identity.account_identity = Some(AccountIdentity {
        email: None,
        display_name: None,
        account_id: Some("fixture-replacement-account".into()),
        plan: None,
        issuer: Some("fixture-issuer".into()),
        captured_at: 1,
        verified: true,
    });
    account_store
        .replace(other_identity)
        .expect("persist replacement identity");
    let other_identity = account_store
        .get(&descriptor.alias)
        .expect("stored replacement");
    // Model-dependent constructor facts can be identical across accounts.
    // A cache hit must still distinguish canonical catalog ownership when
    // the legacy display, credential and capabilities have not changed.
    let profile = view.providers.iter().find(|row| row.provider == PROVIDER);
    let vault = MemoryVault::default();
    vault
        .put(&descriptor.alias, b"fixture-only")
        .expect("fake key");
    let credential = vault.resolve(&descriptor.alias).expect("fake credential");
    let registry = Arc::new(haider_provider::GeminiCacheRegistry::default());
    let tuning = ProviderTuning::default();
    let first = AccountProviderAdapterCacheKey::new(
        profile,
        &descriptor,
        &credential,
        MODEL,
        &tuning,
        None,
        &registry,
    );
    let replaced = AccountProviderAdapterCacheKey::new(
        profile,
        other_identity,
        &credential,
        MODEL,
        &tuning,
        None,
        &registry,
    );
    assert!(
        first != replaced,
        "an adapter cache hit must include canonical account identity, not only display identity"
    );
    f.close().await;
}

async fn default_provider_snapshot(f: &mut Fixture) -> ProviderSummaryWire {
    f.connection
        .request(
            RequestId::new("default-provider-snapshot"),
            RequestBody::ProviderList {
                provider: Some(PROVIDER.into()),
            },
        )
        .await
        .expect("provider snapshot request");
    let ResponseBody::ProviderList { mut providers, .. } =
        response_for(&mut f.frames, "default-provider-snapshot").await
    else {
        panic!("provider snapshot response");
    };
    assert_eq!(providers.len(), 1);
    providers.pop().expect("provider snapshot")
}

#[tokio::test]
async fn catalog_owner_default_provider_snapshot_requires_b_discovery() {
    let mut f = Fixture::new(vec![Ok(catalog(2048))], None, false).await;
    f.publish_a().await;
    let summary = default_provider_snapshot(&mut f).await;
    // This is the real TUI's cache-hit predicate in provider_model_refreshes.
    assert!(
        summary.inventory.needs_refresh(),
        "A's fresh catalog must not suppress default B discovery"
    );
    assert!(!summary.models.iter().any(|model| model == MODEL));
    assert!(
        !summary
            .model_details
            .iter()
            .any(|detail| detail.name == MODEL)
    );
    f.close().await;
}

#[tokio::test]
async fn catalog_owner_default_account_snapshot_uses_b_default() {
    let (mut f, default, _) = maintained_default_fixture().await;
    f.connection
        .request(
            RequestId::new("default-account-snapshot"),
            RequestBody::AccountList {
                provider: Some(PROVIDER.into()),
            },
        )
        .await
        .expect("account snapshot request");
    let ResponseBody::AccountList {
        provider_defaults, ..
    } = response_for(&mut f.frames, "default-account-snapshot").await
    else {
        panic!("account snapshot response");
    };
    assert_eq!(
        provider_defaults
            .iter()
            .find(|row| row.provider == PROVIDER)
            .map(|row| row.model.as_str()),
        Some(default.as_str()),
        "default account snapshot must not republish A's model ordering for B"
    );
    f.close().await;
}

#[tokio::test]
async fn catalog_control_owned_global_snapshot_preserves_fresh_hidden_rows() {
    let mut f = hidden_catalog_fixture(true).await;
    let summary = default_provider_snapshot(&mut f).await;
    assert!(
        !summary.inventory.needs_refresh(),
        "an owned active-account catalog remains a valid cache hit"
    );
    assert!(summary.models.is_empty());
    assert_eq!(summary.availability, ProviderAvailabilityWire::Unavailable);
    assert_eq!(f.discoverer.seen.lock().expect("calls").len(), 1);
    f.close().await;
}

async fn global_catalog_mutation(f: &Fixture, kind: &str, model: &str) -> ResponseBody {
    let expected_revision = f
        .hub
        .accounts()
        .expect("facade")
        .expect("accounts")
        .management
        .read()
        .expect("view")
        .revision;
    let (sink, mut frames) = channel_sink();
    let route = LoginRoute {
        request_id: RequestId::new("global-catalog-mutation"),
        sink,
    };
    let command = match kind {
        "default" => AccountCommand::SetDefaultModel(Box::new(SetDefaultModelJob {
            command_id: "owned-global-default".into(),
            provider: f.provider.into(),
            model: model.into(),
            expected_revision,
            route,
        })),
        "configure" => AccountCommand::ConfigureProvider(Box::new(ProviderConfigureJob {
            command_id: "owned-global-configure".into(),
            input: ProviderConfigureInput {
                provider: f.provider.into(),
                api_family: None,
                origin: None,
                auth_requirement: None,
                enabled: true,
                models: Vec::new(),
                default_model: Some(model.into()),
                response_open_timeout_ms: None,
                chunk_idle_timeout_ms: None,
                semantic_progress_timeout_ms: None,
                trust: None,
            },
            probe_secret: None,
            expected_revision,
            route,
        })),
        "trust" => AccountCommand::SetProviderTrust(Box::new(ProviderSetTrustJob {
            command_id: "owned-global-trust".into(),
            provider: f.provider.into(),
            trust: haider_rpc::ProviderTrustWire::Lockdown,
            expected_revision,
            route,
        })),
        _ => panic!("fixture mutation kind"),
    };
    f.actor
        .commands()
        .send(command)
        .await
        .expect("global mutation request");
    response_for(&mut frames, "global-catalog-mutation").await
}

fn mutation_summary(response: ResponseBody) -> ProviderSummaryWire {
    match response {
        ResponseBody::AccountSetDefaultModel { provider, .. }
        | ResponseBody::ProviderConfigure { provider, .. }
        | ResponseBody::ProviderSetTrust { provider, .. } => provider,
        other => panic!("maintained model mutation must succeed: {other:?}"),
    }
}

async fn foreign_global_admission(kind: &str) {
    let f = Fixture::new(vec![Ok(catalog(2048))], None, false).await;
    f.publish_a().await;
    let response = global_catalog_mutation(&f, kind, MODEL).await;
    assert!(
        matches!(response, ResponseBody::Error { ref code, .. }
        if code == ERROR_CODE_INVALID_ARGUMENT),
        "A's remote-only model cannot admit a global default for active B: {response:?}"
    );
    f.close().await;
}

#[tokio::test]
async fn catalog_owner_global_default_rejects_a_only_inventory() {
    foreign_global_admission("default").await;
}

#[tokio::test]
async fn catalog_owner_global_configure_rejects_a_only_inventory() {
    foreign_global_admission("configure").await;
}

async fn foreign_global_metadata(kind: &str) {
    let (f, _, model) = maintained_default_fixture().await;
    let expected = test_provider_registry()
        .summary(PROVIDER, &|_| true)
        .expect("maintained catalog")
        .model_details
        .into_iter()
        .find(|detail| detail.name == model)
        .expect("maintained detail");
    let summary = mutation_summary(global_catalog_mutation(&f, kind, &model).await);
    assert_eq!(
        summary
            .model_details
            .iter()
            .find(|detail| detail.name == model),
        Some(&expected),
        "a global mutation must not return A's limits as B's metadata"
    );
    f.close().await;
}

#[tokio::test]
async fn catalog_owner_global_default_excludes_a_metadata() {
    foreign_global_metadata("default").await;
}

#[tokio::test]
async fn catalog_owner_global_configure_excludes_a_metadata() {
    foreign_global_metadata("configure").await;
}

#[tokio::test]
async fn catalog_owner_global_trust_excludes_a_metadata() {
    foreign_global_metadata("trust").await;
}

async fn owned_global_mutation(kind: &str) {
    let f = Fixture::new(vec![Ok(catalog(2048))], None, false).await;
    let (sink, mut frames) = channel_sink();
    f.actor
        .commands()
        .send(AccountCommand::SetActive(Box::new(SetActiveJob {
            command_id: "owned-global-active-a".into(),
            alias: "a".into(),
            route: LoginRoute {
                request_id: RequestId::new("owned-global-active-a"),
                sink,
            },
        })))
        .await
        .expect("activate A");
    assert!(matches!(
        response_for(&mut frames, "owned-global-active-a").await,
        ResponseBody::AccountSetActive { .. }
    ));
    f.publish_a().await;
    let summary = mutation_summary(global_catalog_mutation(&f, kind, MODEL).await);
    assert_eq!(
        summary
            .model_details
            .iter()
            .find(|detail| detail.name == MODEL)
            .and_then(|detail| detail.max_output_tokens),
        Some(2048),
        "an owned global mutation preserves the active account's remote metadata"
    );
    f.close().await;
}

#[tokio::test]
async fn catalog_control_owned_global_default_preserves_metadata() {
    owned_global_mutation("default").await;
}

#[tokio::test]
async fn catalog_control_owned_global_configure_preserves_metadata() {
    owned_global_mutation("configure").await;
}

#[tokio::test]
async fn catalog_control_owned_global_trust_preserves_metadata() {
    owned_global_mutation("trust").await;
}

#[tokio::test]
async fn catalog_owner_staged_a_probe_cannot_cache_limits_for_active_b() {
    let f = Fixture::custom(vec![Ok(catalog(2048))]).await;
    let (sink, mut frames) = channel_sink();
    f.actor
        .commands()
        .send(AccountCommand::ConfigureProvider(Box::new(
            ProviderConfigureJob {
                command_id: "staged-a-catalog".into(),
                input: ProviderConfigureInput {
                    provider: f.provider.into(),
                    api_family: None,
                    origin: None,
                    auth_requirement: None,
                    enabled: true,
                    models: Vec::new(),
                    default_model: Some(MODEL.into()),
                    response_open_timeout_ms: None,
                    chunk_idle_timeout_ms: None,
                    semantic_progress_timeout_ms: None,
                    trust: None,
                },
                probe_secret: Some(zeroize::Zeroizing::new(b"a".to_vec())),
                expected_revision: 0,
                route: LoginRoute {
                    request_id: RequestId::new("staged-a-catalog"),
                    sink,
                },
            },
        )))
        .await
        .expect("staged probe request");
    tokio::time::timeout(
        haider_platform::TEST_WAIT_TIMEOUT,
        f.discoverer.started.notified(),
    )
    .await
    .expect("staged probe started");
    f.discoverer.release.notify_one();
    let summary = mutation_summary(response_for(&mut frames, "staged-a-catalog").await);
    assert!(
        summary
            .model_details
            .iter()
            .all(|detail| detail.max_output_tokens != Some(2048)),
        "a staged A credential cannot publish remote limits as active B's metadata"
    );
    let descriptor = f
        .hub
        .accounts()
        .expect("facade")
        .expect("accounts")
        .management
        .read()
        .expect("view")
        .descriptors
        .into_iter()
        .find(|row| row.alias.as_str() == "b")
        .expect("B descriptor");
    let key = ProviderModelCacheKey::for_account(f.provider, &descriptor);
    assert!(
        f.store
            .provider_models(key.into())
            .await
            .expect("B cache read")
            .is_none(),
        "a staged A probe cannot create a B-owned cache row"
    );
    f.close().await;
}

async fn recovered_catalog_is_account_bound(public: bool) {
    let f = Fixture::custom_with_recovery(Vec::new(), Some(catalog(2048)), public).await;
    let view = tokio::time::timeout(haider_platform::TEST_WAIT_TIMEOUT, async {
        loop {
            let view = f
                .hub
                .accounts()
                .expect("facade")
                .expect("accounts")
                .management
                .read()
                .expect("view");
            if view.revision > 0 {
                break view;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("recovered management publication");
    let summary = view
        .providers
        .iter()
        .find(|row| row.provider == f.provider)
        .expect("recovered provider");
    assert_eq!(
        summary
            .model_details
            .iter()
            .find(|detail| detail.name == MODEL)
            .and_then(|detail| detail.max_output_tokens),
        public.then_some(2048),
        "recovery cannot relabel an unowned private catalog as active B's metadata"
    );
    let key = if public {
        ProviderModelCacheKey::public(f.provider)
    } else {
        ProviderModelCacheKey::for_account(
            f.provider,
            view.descriptors
                .iter()
                .find(|row| row.alias.as_str() == "b")
                .expect("B descriptor"),
        )
    };
    assert_eq!(
        f.store
            .provider_models(key.into())
            .await
            .expect("recovered cache")
            .is_some(),
        public,
        "only provider-wide recovery can restore a catalog without an account owner"
    );
    f.close().await;
}

#[tokio::test]
async fn catalog_owner_recovery_cannot_relabel_a_catalog_as_b() {
    recovered_catalog_is_account_bound(false).await;
}

#[tokio::test]
async fn catalog_control_public_recovery_preserves_remote_metadata() {
    recovered_catalog_is_account_bound(true).await;
}

async fn configured_custom_global_default(check_owner: bool) {
    let f = Fixture::custom(vec![Ok(catalog(2048))]).await;
    f.publish_a().await;
    let summary = mutation_summary(global_catalog_mutation(&f, "default", MODEL).await);
    assert_eq!(
        summary.default_model.as_deref(),
        Some(MODEL),
        "the provider's explicit configured model remains a valid custom default"
    );
    if check_owner {
        assert_eq!(
            summary
                .model_details
                .iter()
                .find(|detail| detail.name == MODEL)
                .and_then(|detail| detail.max_output_tokens),
            None,
            "a configured B default cannot inherit A's remote output ceiling"
        );
    }
    f.close().await;
}

#[tokio::test]
async fn catalog_owner_configured_custom_default_excludes_a_metadata() {
    configured_custom_global_default(true).await;
}

#[tokio::test]
async fn catalog_control_configured_custom_default_survives_foreign_catalog() {
    configured_custom_global_default(false).await;
}

async fn provider_command_uses_owning_default(owned: bool) {
    let baseline = test_provider_registry()
        .summaries(&|_| true)
        .into_iter()
        .find(|summary| summary.provider == PROVIDER)
        .expect("maintained provider");
    let default = baseline.default_model.expect("B default");
    let alternate = baseline
        .models
        .into_iter()
        .find(|model| model != &default)
        .expect("alternate");
    let mut a = catalog(8192);
    a.models[0].slug = alternate.clone();
    a.models[0].display_name = alternate.clone();
    a.models[0].priority = Some(-1000);
    let mut b = catalog(8192);
    b.models[0].slug = default.clone();
    b.models[0].display_name = default.clone();
    let mut f = Fixture::new(vec![Ok(a), Ok(b)], owned.then_some("a"), false).await;
    f.publish_a().await;
    f.discoverer.release.notify_one();
    f.connection
        .request(
            RequestId::new("provider-default"),
            RequestBody::CommandInvoke {
                command_id: CommandId::new("provider-default"),
                command: format!("/provider {PROVIDER}"),
                session_id: Some(f.session.clone()),
            },
        )
        .await
        .expect("provider command");
    let response = response_for(&mut f.frames, "provider-default").await;
    assert!(
        matches!(
            response,
            ResponseBody::CommandInvoke {
                outcome: haider_rpc::CommandInvokeOutcomeWire::Receipt { .. }
            }
        ),
        "provider command must complete: {response:?}"
    );
    let metadata = f
        .hub
        .session_metadata(&f.session)
        .await
        .expect("durable read")
        .expect("session");
    assert_eq!(
        metadata.model,
        if owned { alternate } else { default },
        "provider command must choose its account's default, not another account's priority"
    );
    assert_eq!(metadata.account_alias.as_deref(), owned.then_some("a"));
    assert_eq!(
        f.discoverer.seen.lock().expect("calls").len(),
        if owned { 1 } else { 2 },
        "foreign default requires B discovery; owned default remains a cache hit"
    );
    f.close().await;
}

#[tokio::test]
async fn catalog_owner_provider_command_uses_b_default() {
    provider_command_uses_owning_default(false).await;
}

#[tokio::test]
async fn catalog_control_provider_command_preserves_owned_default() {
    provider_command_uses_owning_default(true).await;
}

async fn public_catalog_ignores_account_owner(cached: bool, switch_active: bool) {
    let mut results = vec![Ok(catalog(2048))];
    if cached {
        results.push(Err(CatalogError::NotModified));
    }
    let mut f = Fixture::custom_with_recovery(results, None, true).await;
    if cached {
        f.publish_a().await;
    }
    // Public discovery must ignore even an alias that has no descriptor.
    let completion = f.begin_refresh_for("not-a-public-account").await;
    if switch_active {
        let (sink, mut frames) = channel_sink();
        f.actor
            .commands()
            .send(AccountCommand::SetActive(Box::new(SetActiveJob {
                command_id: "public-active-change".into(),
                alias: "a".into(),
                route: LoginRoute {
                    request_id: RequestId::new("public-active-change"),
                    sink,
                },
            })))
            .await
            .expect("public active switch");
        assert!(
            matches!(
                response_for(&mut frames, "public-active-change").await,
                ResponseBody::AccountSetActive { .. }
            ),
            "active switch must commit during public discovery"
        );
    }
    f.discoverer.release.notify_one();
    completion
        .await
        .expect("public completion")
        .expect("public catalog is not bound to the ignored account alias");
    f.connection
        .request(
            RequestId::new("public-cached-selection"),
            f.selection(false, Some(1024)),
        )
        .await
        .expect("public selection");
    let response = response_for(&mut f.frames, "public-cached-selection").await;
    assert!(
        matches!(response, ResponseBody::SessionSelectModel { .. }),
        "public catalog must remain selectable without account discovery: {response:?}"
    );
    {
        let seen = f.discoverer.seen.lock().expect("public observations");
        assert_eq!(
            seen.len(),
            if cached { 2 } else { 1 },
            "the public cache remains a hit across account coordinates"
        );
        assert!(
            seen.iter().all(|row| row.1.is_none()),
            "public discovery never consumes an account credential"
        );
    }
    assert!(
        f.store
            .provider_models(ProviderModelCacheKey::public(f.provider).into())
            .await
            .expect("public cache")
            .is_some()
    );
    f.close().await;
}

#[tokio::test]
async fn catalog_control_public_success_ignores_explicit_alias() {
    public_catalog_ignores_account_owner(false, false).await;
}

#[tokio::test]
async fn catalog_control_public_304_ignores_explicit_alias() {
    public_catalog_ignores_account_owner(true, false).await;
}

#[tokio::test]
async fn catalog_control_public_flight_survives_active_account_change() {
    public_catalog_ignores_account_owner(false, true).await;
}

#[tokio::test]
async fn catalog_owner_inactive_rejection_persists_without_poisoning_active_catalog() {
    let registry_dir = test_store_dir();
    let providers = ProviderRegistry::new(
        Box::new(crate::provider_registry::JsonProviderRegistryStore::new(
            registry_dir.path(),
        )) as Box<dyn ProviderRegistryStoreLike>,
        crate::provider_registry::initial_provider_profiles(
            &std::collections::BTreeSet::from([PROVIDER.to_owned()]),
            INITIAL_MODEL,
        ),
        Arc::new(CachedProviderModelSource::default()),
    )
    .expect("persistent fixture registry");
    let fixture = Fixture::start(
        vec![Ok(catalog(8192)), Err(CatalogError::Unauthorized)],
        None,
        false,
        PROVIDER,
        AuthMethod::OAuth,
        providers,
        None,
    )
    .await;
    let ready = fixture.begin_refresh_for("b").await;
    fixture.discoverer.release.notify_one();
    ready.await.expect("B completion").expect("B catalog");
    let facade = fixture
        .hub
        .accounts()
        .expect("account facade")
        .expect("installed facade");
    let before = facade.management.read().expect("before snapshot");
    let expected_scope = |alias: &str| {
        let descriptor = before
            .descriptors
            .iter()
            .find(|row| row.alias.as_str() == alias)
            .expect("fixture account");
        let wire = serde_json::to_value(descriptor).expect("public descriptor");
        let identity = serde_json::json!({
            "v": 1, "profile": "catalog-owner-test", "provider": PROVIDER,
            "source": format!("{:?}", CatalogSource::OpenAiSubscription),
            "origin": before.providers.iter().find(|row| row.provider == PROVIDER)
                .and_then(|row| row.endpoint.clone()),
            "auth": ProviderAuthRequirementWire::OAuth, "alias": descriptor.alias,
            "incarnation": wire.get("incarnation"),
            "cache_key": String::from(ProviderModelCacheKey::for_account(PROVIDER, descriptor)),
        });
        blake3::hash(identity.to_string().as_bytes())
            .to_hex()
            .to_string()
    };
    let a_scope = expected_scope("a");
    let b_scope = expected_scope("b");
    assert_ne!(a_scope, b_scope);
    let rejected = fixture.begin_refresh_for("a").await;
    fixture.discoverer.release.notify_one();
    assert!(rejected.await.expect("A completion").is_err());
    let after = facade.management.read().expect("after snapshot");
    assert_eq!(
        before.providers, after.providers,
        "an inactive A rejection cannot poison active B's published catalog"
    );
    let document: serde_json::Value = serde_json::from_slice(
        &std::fs::read(registry_dir.path().join("providers.json")).expect("registry file"),
    )
    .expect("registry document");
    let evidence = document
        .get("catalog_evidence")
        .and_then(serde_json::Value::as_object)
        .expect("inactive account rejection must remain durable under its producing account");
    assert!(
        evidence.contains_key(&a_scope),
        "A rejection must retain A's original producer"
    );
    assert!(
        !evidence.contains_key(&b_scope),
        "A rejection cannot be cached as active B's evidence"
    );
    fixture.close().await;
}
