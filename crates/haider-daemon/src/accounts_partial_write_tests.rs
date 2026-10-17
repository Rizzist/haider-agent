#![allow(clippy::expect_used)]

use super::*;

struct UnavailableCatalog;
#[async_trait::async_trait]
impl ProviderModelDiscoverer for UnavailableCatalog {
    async fn discover(
        &self,
        _: CatalogSource,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<DiscoveredCatalog, CatalogError> {
        Err(CatalogError::Transport {
            reason: "synthetic discovery outage".into(),
        })
    }
}

fn write_source(kind: CredentialSourceKind, root: &std::path::Path, account: &str) {
    match kind {
        CredentialSourceKind::CodexHome => {
            write_linked_codex_source(root, account, &format!("{account}@synthetic.invalid"))
        }
        CredentialSourceKind::GrokHome => {
            write_linked_grok_source(root, account, &format!("{account}@synthetic.invalid"))
        }
        CredentialSourceKind::KimiCodeHome => {
            write_linked_kimi_source(root, account, "kimi-code.json")
        }
        CredentialSourceKind::ClaudeFile => {
            unreachable!("Claude requires the origin client and has no remote catalog admission")
        }
    }
}

fn source_file(kind: CredentialSourceKind, root: &std::path::Path) -> std::path::PathBuf {
    match kind {
        CredentialSourceKind::KimiCodeHome => root.join("credentials/kimi-code.json"),
        _ => root.join("auth.json"),
    }
}

fn fetched_model(slug: &str) -> DiscoveredModel {
    serde_json::from_value(serde_json::json!({"slug":slug,"display_name":slug,"supported_efforts":[],"visible":true,"use_responses_lite":true})).expect("synthetic fetched model")
}

fn source_actor(
    store: SqliteStoreHandle,
    accounts: AccountStore<Box<dyn StoreLike>>,
    providers: ProviderRegistry<Box<dyn ProviderRegistryStoreLike>>,
    registry: CredentialSourceRegistry,
    management: ManagementSnapshot,
) -> AccountActorHandle {
    let snapshot = Arc::new(StdMutex::new(accounts.list().to_vec()));
    let sources = Arc::new(StdMutex::new(account_source_wires(&registry, &accounts)));
    let (commands, receiver) = mpsc::channel(ACTOR_CAPACITY);
    let (force_stop, forced) = watch::channel(false);
    spawn_account_actor(
        AccountActorConfig {
            store,
            accounts,
            providers,
            snapshot,
            management: Some(management),
            vault: Arc::new(MemoryVault::new()),
            validator: Arc::new(ProviderCredentialValidator),
            device_discovery: DeviceDiscoverySnapshot::new(true),
            profile_id: "partial-write".into(),
            default_model: "unused".into(),
            provider_endpoint_validator: Arc::new(CustomprovCanonicalEndpoint),
            reserved_aliases: HashSet::new(),
            refresh_fences: RefreshFenceRegistry::default(),
            source_registry: Arc::new(StdMutex::new(registry)),
            source_snapshot: sources,
        },
        commands,
        receiver,
        force_stop,
        forced,
        None,
        Arc::new(UnavailableCatalog),
        Arc::new(UnreachableGcloud),
        Arc::new(StrictNoNativeCredentialStore),
    )
}

async fn source_case(kind: CredentialSourceKind, entry: &str, same_account: bool) {
    let dir = test_store_dir();
    let root = dir.path().join("source");
    write_source(kind, &root, "synthetic-A");
    let mut registry = CredentialSourceRegistry::load(dir.path()).expect("registry");
    let enrolled = registry.enroll(kind, &root, None).expect("enroll");
    let mut accounts = AccountStore::new(Box::new(DescriptorStore::Json(JsonFileStore::new(
        dir.path(),
    ))) as Box<dyn StoreLike>)
    .expect("durable accounts");
    reconcile_credential_sources(&mut registry, &mut accounts).expect("initial scan");
    let provider = source_provider(kind);
    let a = accounts.active_for_provider(provider).expect("A").clone();
    let mut providers = ProviderRegistry::new(
        Box::new(TestProviderStore::default()) as Box<dyn ProviderRegistryStoreLike>,
        initial_provider_profiles(
            &std::collections::BTreeSet::from([provider.to_owned()]),
            "unused",
        ),
        Arc::new(CachedProviderModelSource::default()),
    )
    .expect("providers");
    providers.replace_discovered_models(provider.into(), vec![fetched_model("a-private-only")], 1);
    reconcile_catalog_selection_scope(provider, &accounts, &mut providers);
    providers
        .set_default_model(provider, "a-private-only")
        .expect("A default");
    let management = ManagementSnapshot::new(
        0,
        accounts.list().to_vec(),
        catalog_summaries(&accounts, &providers),
    );
    assert!(
        management
            .read()
            .expect("positive control")
            .providers
            .iter()
            .any(|p| p.models.contains(&"a-private-only".into()))
    );
    let store = open_store(dir.path()).await;
    let mut actor = source_actor(
        store.clone(),
        accounts,
        providers,
        registry,
        management.clone(),
    );
    let obstacle = dir.path().join("credential_sources.json.tmp");
    std::fs::create_dir(&obstacle).expect("metadata write obstacle");
    if entry == "gone" {
        std::fs::remove_file(source_file(kind, &root)).expect("source removed");
    } else if entry != "remove" {
        write_source(
            kind,
            &root,
            if same_account {
                "synthetic-A"
            } else {
                "synthetic-B"
            },
        );
    }
    let (sink, mut frames) = channel_sink();
    let route = LoginRoute {
        request_id: RequestId::new("partial-write"),
        sink,
    };
    let command = match entry {
        "scan" | "gone" => AccountCommand::SourceScan { completed: route },
        "add" => AccountCommand::SourceAdd(Box::new(SourceAddJob {
            kind: kind.as_str().into(),
            root: root.display().to_string(),
            label: None,
            route,
        })),
        "remove" => AccountCommand::SourceRemove(Box::new(SourceRemoveJob {
            source_id: enrolled.id,
            route,
        })),
        "watch" => AccountCommand::ReconcileSources,
        "read-through" => {
            let (completed, received) = tokio::sync::oneshot::channel();
            actor
                .commands()
                .send(AccountCommand::BeginOAuthImportHeal {
                    descriptor: a.clone(),
                    expected: OAuthRefreshFence {
                        fence_epoch: 0,
                        generation: 0,
                        issuer: "synthetic".into(),
                        audience: "synthetic".into(),
                        resource: None,
                        subject_hash: "synthetic".into(),
                    },
                    completed,
                })
                .await
                .expect("read-through heal");
            assert!(
                tokio::time::timeout(Duration::from_secs(10), received)
                    .await
                    .expect("heal deadline")
                    .expect("heal result")
                    .is_err()
            );
            AccountCommand::SourceList { completed: route }
        }
        _ => panic!("unknown entry"),
    };
    actor
        .commands()
        .send(command)
        .await
        .expect("source command");
    if entry == "watch" {
        let (sink, mut barrier) = channel_sink();
        actor
            .commands()
            .send(AccountCommand::SourceList {
                completed: LoginRoute {
                    request_id: RequestId::new("barrier"),
                    sink,
                },
            })
            .await
            .expect("barrier");
        tokio::time::timeout(Duration::from_secs(10), barrier.recv())
            .await
            .expect("barrier deadline")
            .expect("barrier reply");
    } else {
        let reply = tokio::time::timeout(Duration::from_secs(10), frames.recv())
            .await
            .expect("command deadline")
            .expect("reply");
        if entry != "read-through" {
            assert!(
                matches!(
                    reply,
                    WireFrame::Response {
                        body: ResponseBody::Error { .. },
                        ..
                    }
                ),
                "actual metadata write failure: {reply:?}"
            );
        }
    }
    let persisted = JsonFileStore::new(dir.path())
        .load()
        .expect("durable descriptor");
    let b = persisted
        .iter()
        .find(|row| row.provider == provider && row.active)
        .expect("committed active account");
    assert_eq!(
        b.incarnation == a.incarnation,
        same_account,
        "the descriptor commit precedes the source metadata error"
    );
    let view = management.read().expect("coherent management snapshot");
    let current = view
        .providers
        .iter()
        .find(|p| p.provider == provider)
        .expect("provider");
    let retained = current.models.contains(&"a-private-only".into())
        || current.default_model.as_deref() == Some("a-private-only");
    // Shut down before asserting, so a failing base run leaves no owned actor.
    actor.shutdown().await;
    store.close().await.expect("close");
    assert_eq!(
        retained, same_account,
        "a committed source account change must retire A rows and default even when later metadata fails ({entry})"
    );
}

#[tokio::test]
async fn partial_write_source_scan() {
    source_case(CredentialSourceKind::CodexHome, "scan", false).await;
}
#[tokio::test]
async fn partial_write_source_existing_add() {
    source_case(CredentialSourceKind::CodexHome, "add", false).await;
}
#[tokio::test]
async fn partial_write_source_watcher() {
    source_case(CredentialSourceKind::CodexHome, "watch", false).await;
}
#[tokio::test]
async fn partial_write_source_read_through() {
    source_case(CredentialSourceKind::CodexHome, "read-through", false).await;
}
#[tokio::test]
async fn partial_write_source_remove() {
    source_case(CredentialSourceKind::CodexHome, "remove", false).await;
}
#[tokio::test]
async fn partial_write_source_missing() {
    source_case(CredentialSourceKind::CodexHome, "gone", false).await;
}
#[tokio::test]
async fn partial_write_source_grok() {
    source_case(CredentialSourceKind::GrokHome, "scan", false).await;
}
#[tokio::test]
async fn partial_write_source_kimi() {
    source_case(CredentialSourceKind::KimiCodeHome, "scan", false).await;
}
#[tokio::test]
async fn partial_write_source_same_account_control() {
    source_case(CredentialSourceKind::CodexHome, "scan", true).await;
}

async fn configuration_case(repoint: bool, overflow: bool, permanent: bool) {
    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    create_provider_trust_test_session(&store, dir.path(), "partial-auth-session").await;
    let origin = "https://old.synthetic.invalid/v1";
    let mut profile = endpoint_edit_profile(OPENAI_PROVIDER_NAME, origin);
    profile.configured_models = if permanent {
        vec!["configured-deployment".into()]
    } else {
        vec![]
    };
    profile.default_model = None;
    let backing = EndpointEditProviderStore::new(vec![profile]);
    let mut providers = ProviderRegistry::new(
        Box::new(backing.clone()) as Box<dyn ProviderRegistryStoreLike>,
        vec![],
        Arc::new(CachedProviderModelSource::default()),
    )
    .expect("provider registry");
    let mut accounts = memory_accounts();
    accounts
        .add(CredentialDescriptor {
            incarnation: 0,
            credential_digest: Some("synthetic-A".into()),
            alias: CredentialAlias::new("work"),
            provider: OPENAI_PROVIDER_NAME.into(),
            base_url: Some(origin.into()),
            auth_method: AuthMethod::ApiKey,
            identity: "synthetic A".into(),
            status: CredentialStatus::Ok,
            active: true,
            label: None,
            account_identity: None,
            created_at_ms: None,
        })
        .expect("account A");
    providers.replace_discovered_models(
        OPENAI_PROVIDER_NAME.into(),
        vec![fetched_model("a-private-only")],
        1,
    );
    reconcile_catalog_selection_scope(OPENAI_PROVIDER_NAME, &accounts, &mut providers);
    let chosen = if permanent {
        "configured-deployment"
    } else {
        "a-private-only"
    };
    providers
        .set_default_model(OPENAI_PROVIDER_NAME, chosen)
        .expect("default positive control");
    let management = ManagementSnapshot::new(
        0,
        accounts.list().to_vec(),
        catalog_summaries(&accounts, &providers),
    );
    assert!(
        management
            .read()
            .expect("view")
            .providers
            .iter()
            .any(|p| p.models.contains(&"a-private-only".into()))
    );
    let mut input = endpoint_repoint_input(
        OPENAI_PROVIDER_NAME,
        if repoint {
            "https://new.synthetic.invalid/v1"
        } else {
            origin
        },
    );
    input.auth_requirement = Some(ProviderAuthRequirementWire::None);
    input.models = if permanent {
        vec![chosen.into()]
    } else {
        vec![]
    };
    input.default_model = None;
    let expected_revision = if overflow { u64::MAX } else { 0 };
    let command_id = "partial-auth-save";
    if overflow {
        // A pending receipt bypasses a fresh expected-revision check and
        // exercises the checked-add return after the profile has committed.
        let identity = ProviderConfigureIdentity {
            input: input.clone(),
            expected_revision,
        };
        let (request_json, request_digest) = command_json(&identity).expect("identity");
        let recovery = ProviderConfigureRecovery {
            input: input.clone(),
            previous_auth_requirement: Some(ProviderAuthRequirementWire::ApiKey),
            previous_origin: Some(origin.into()),
            discovered_models: None,
            discovered_etag: None,
            catalog_producer: None,
            revision_unchanged: false,
            revision_unchanged_response: None,
        };
        store
            .management_claim_receipt::<ProviderReceipt>(
                command_id.into(),
                PROVIDER_CONFIGURE_METHOD.into(),
                request_digest,
                request_json,
                Some(serde_json::to_string(&recovery).expect("recovery")),
                None,
            )
            .await
            .expect("pending receipt");
    } else {
        let db =
            rusqlite::Connection::open(dir.path().join("store.sqlite")).expect("fixture database");
        db.execute_batch("CREATE TRIGGER partial_auth_failure BEFORE INSERT ON events WHEN NEW.event_id LIKE '%provider-auth-%' BEGIN SELECT RAISE(ABORT, 'synthetic journal unavailable'); END;").expect("inject journal failure");
    }
    let snapshot = Arc::new(StdMutex::new(accounts.list().to_vec()));
    let mut pending = AutomaticCatalogDiscoveryQueue::new(true);
    let vault = MemoryVault::new();
    let (sink, mut frames) = channel_sink();
    handle_provider_configure(
        ProviderConfigureContext {
            profile_id: "partial-auth",
            store: &store,
            accounts: &mut accounts,
            snapshot: &snapshot,
            vault: &vault,
            management: Some(&management),
            providers: &mut providers,
            pending_catalog_discoveries: &mut pending,
            endpoint_validator: Arc::new(CustomprovCanonicalEndpoint),
            model_discoverer: &UnavailableCatalog,
        },
        ProviderConfigureJob {
            command_id: command_id.into(),
            input,
            probe_secret: None,
            expected_revision,
            route: LoginRoute {
                request_id: RequestId::new(command_id),
                sink,
            },
        },
    )
    .await;
    let reply = frames.recv().await.expect("configure reply");
    let WireFrame::Response {
        body: ResponseBody::Error { message, .. },
        ..
    } = reply
    else {
        panic!("expected journal/revision failure: {reply:?}")
    };
    assert!(
        message.contains(if overflow {
            "revision space is exhausted"
        } else {
            "synthetic journal unavailable"
        }),
        "intended late failure: {message}"
    );
    assert_eq!(
        backing.snapshot()[0].auth_requirement,
        ProviderAuthRequirementWire::None,
        "saved configuration control"
    );
    let direct =
        catalog_summary(OPENAI_PROVIDER_NAME, &accounts, &providers).expect("current provider");
    let published = management.read().expect("admission view");
    let published = published
        .providers
        .iter()
        .find(|p| p.provider == OPENAI_PROVIDER_NAME)
        .expect("provider");
    let retired = !direct.models.contains(&"a-private-only".into())
        && direct.default_model.as_deref() != Some("a-private-only")
        && !published.models.contains(&"a-private-only".into())
        && published.default_model.as_deref() != Some("a-private-only");
    if permanent {
        assert_eq!(direct.default_model.as_deref(), Some(chosen));
        assert!(direct.models.contains(&chosen.into()));
    }
    store.close().await.expect("close");
    assert!(
        retired,
        "saved auth/endpoint changes retire fetched rows and default in both actor and admission views before any late error"
    );
}
#[tokio::test]
async fn partial_write_configuration_journal() {
    configuration_case(false, false, false).await;
}
#[tokio::test]
async fn partial_write_configuration_endpoint_and_auth() {
    configuration_case(true, false, false).await;
}
#[tokio::test]
async fn partial_write_configuration_revision_overflow() {
    configuration_case(false, true, false).await;
}
#[tokio::test]
async fn partial_write_configuration_preserves_explicit_deployment() {
    configuration_case(true, false, true).await;
}

struct LimitedSourceWrites {
    backing: JsonFileStore,
    remaining: Arc<std::sync::atomic::AtomicUsize>,
}
impl StoreLike for LimitedSourceWrites {
    fn reserve_incarnation(&self, floor: u64) -> Result<u64, HaiderError> {
        self.backing.reserve_incarnation(floor)
    }
    fn load(&self) -> Result<Vec<CredentialDescriptor>, HaiderError> {
        self.backing.load()
    }
    fn save(&self, descriptors: &[CredentialDescriptor]) -> Result<(), HaiderError> {
        self.remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                count.checked_sub(1)
            })
            .map_err(|_| {
                HaiderError::new(
                    ErrorCode::StoreLocked,
                    "synthetic later source save failure",
                    true,
                )
            })?;
        self.backing.save(descriptors)
    }
}

#[tokio::test]
async fn partial_write_earlier_source_commit_survives_later_source_error() {
    let dir = test_store_dir();
    let mut registry = CredentialSourceRegistry::load(dir.path()).expect("registry");
    for kind in [
        CredentialSourceKind::GrokHome,
        CredentialSourceKind::KimiCodeHome,
    ] {
        let root = dir.path().join(kind.as_str());
        write_source(kind, &root, "synthetic-A");
        registry.enroll(kind, &root, None).expect("enroll");
    }
    let remaining = Arc::new(std::sync::atomic::AtomicUsize::new(usize::MAX));
    let mut accounts = AccountStore::new(Box::new(LimitedSourceWrites {
        backing: JsonFileStore::new(dir.path()),
        remaining: remaining.clone(),
    }) as Box<dyn StoreLike>)
    .expect("durable accounts");
    reconcile_credential_sources(&mut registry, &mut accounts).expect("initial scan");
    let first_provider = source_provider(registry.records()[0].kind);
    let original = accounts
        .active_for_provider(first_provider)
        .expect("first A")
        .clone();
    let mut providers = ProviderRegistry::new(
        Box::new(TestProviderStore::default()) as Box<dyn ProviderRegistryStoreLike>,
        initial_provider_profiles(
            &std::collections::BTreeSet::from([
                GROK_OAUTH_PROVIDER_NAME.into(),
                KIMI_OAUTH_PROVIDER_NAME.into(),
            ]),
            "unused",
        ),
        Arc::new(CachedProviderModelSource::default()),
    )
    .expect("providers");
    providers.replace_discovered_models(
        first_provider.into(),
        vec![fetched_model("a-private-only")],
        1,
    );
    reconcile_catalog_selection_scope(first_provider, &accounts, &mut providers);
    providers
        .set_default_model(first_provider, "a-private-only")
        .expect("A default");
    let management = ManagementSnapshot::new(
        0,
        accounts.list().to_vec(),
        catalog_summaries(&accounts, &providers),
    );
    for record in registry.records() {
        write_source(
            record.kind,
            std::path::Path::new(&record.root),
            "synthetic-B",
        );
    }
    remaining.store(1, Ordering::SeqCst);
    let store = open_store(dir.path()).await;
    let mut actor = source_actor(
        store.clone(),
        accounts,
        providers,
        registry,
        management.clone(),
    );
    let (sink, mut replies) = channel_sink();
    actor
        .commands()
        .send(AccountCommand::SourceScan {
            completed: LoginRoute {
                request_id: RequestId::new("later-source-error"),
                sink,
            },
        })
        .await
        .expect("scan");
    let reply = tokio::time::timeout(Duration::from_secs(10), replies.recv())
        .await
        .expect("deadline")
        .expect("reply");
    assert!(
        matches!(
            reply,
            WireFrame::Response {
                body: ResponseBody::Error { .. },
                ..
            }
        ),
        "later source save fails: {reply:?}"
    );
    let persisted = JsonFileStore::new(dir.path())
        .load()
        .expect("durable accounts");
    assert!(
        persisted
            .iter()
            .any(|row| row.provider == first_provider && row.incarnation > original.incarnation),
        "the earlier source already committed"
    );
    let view = management.read().expect("view");
    let first = view
        .providers
        .iter()
        .find(|p| p.provider == first_provider)
        .expect("first provider");
    let retired = !first.models.contains(&"a-private-only".into())
        && first.default_model.as_deref() != Some("a-private-only");
    actor.shutdown().await;
    store.close().await.expect("close");
    assert!(
        retired,
        "an error from a later source cannot retain an earlier source's retired catalog"
    );
}
