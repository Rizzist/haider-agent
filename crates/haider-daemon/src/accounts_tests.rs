//! Unit tests for the daemon account seam: physical-alias namespacing,
//! staged-secret laws, the login receipt shape, and the R10 startup
//! reconciliation over every crash boundary.
#![allow(clippy::expect_used)]

use super::*;
use haider_core::SqliteStoreHandle;

use crate::provider_registry::ProviderProfileV1;
use crate::session_hub::FrameSendError;
use crate::worker::ProviderFactory as _;
use haider_core::ProviderAttemptResolver as _;
use haider_rpc::{ProviderApiFamilyWire, ProviderAuthRequirementWire};

fn test_store_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("hacct")
        .tempdir_in("/tmp")
        .unwrap_or_else(|error| panic!("tempdir: {error}"))
}

fn memory_accounts() -> AccountStore<Box<dyn StoreLike>> {
    struct Ephemeral(StdMutex<Vec<CredentialDescriptor>>);
    impl StoreLike for Ephemeral {
        fn load(&self) -> Result<Vec<CredentialDescriptor>, HaiderError> {
            Ok(self.0.lock().map(|view| view.clone()).unwrap_or_default())
        }
        fn save(&self, descriptors: &[CredentialDescriptor]) -> Result<(), HaiderError> {
            if let Ok(mut view) = self.0.lock() {
                *view = descriptors.to_vec();
            }
            Ok(())
        }
    }
    let store: Box<dyn StoreLike> = Box::new(Ephemeral(StdMutex::new(Vec::new())));
    AccountStore::new(store).unwrap_or_else(|error| panic!("accounts: {error:?}"))
}

#[derive(Default)]
struct TestProviderStore(StdMutex<Vec<ProviderProfileV1>>);

impl ProviderRegistryStoreLike for TestProviderStore {
    fn load(&self) -> Result<Vec<ProviderProfileV1>, HaiderError> {
        Ok(self.0.lock().map(|view| view.clone()).unwrap_or_default())
    }

    fn save(&self, profiles: &[ProviderProfileV1]) -> Result<(), HaiderError> {
        if let Ok(mut view) = self.0.lock() {
            *view = profiles.to_vec();
        }
        Ok(())
    }
}

fn test_provider_registry() -> ProviderRegistry<Box<dyn ProviderRegistryStoreLike>> {
    let store: Box<dyn ProviderRegistryStoreLike> = Box::new(TestProviderStore::default());
    ProviderRegistry::new(
        store,
        initial_provider_profiles(
            &std::collections::BTreeSet::from([
                ANTHROPIC_PROVIDER_NAME.to_owned(),
                OPENAI_PROVIDER_NAME.to_owned(),
            ]),
            "claude-test",
        ),
    )
    .expect("provider registry")
}

fn identity_for(profile: &str, command: &str) -> LoginIdentity {
    LoginIdentity {
        provider: "anthropic".into(),
        resolved_model: "claude-test".into(),
        display_alias: Some("work".into()),
        physical_alias: physical_alias(profile, "anthropic", command),
    }
}

fn oauth_identity_for(profile: &str, command: &str) -> OAuthAddIdentity {
    OAuthAddIdentity {
        provider: "fake-oauth".into(),
        display_alias: "work-oauth".into(),
        physical_alias: physical_alias(profile, "fake-oauth", command),
        auth_method: "oauth".into(),
    }
}

fn oauth_bundle() -> haider_accounts::OAuthTokenBundleV1 {
    haider_accounts::OAuthTokenBundleV1::new(
        "fake-oauth".into(),
        "http://127.0.0.1:32109".into(),
        "fake-audience".into(),
        Some("fake-api-resource".into()),
        "Bearer".into(),
        Zeroizing::new(b"OAUTH_ACCESS_RECEIPT_SENTINEL_1fa2".to_vec()),
        Some(Zeroizing::new(
            b"OAUTH_REFRESH_RECEIPT_SENTINEL_9c31".to_vec(),
        )),
        u64::MAX - 1,
        Some(u64::MAX),
        vec!["openid".into(), "inference".into()],
        haider_accounts::OAuthIdentityV1 {
            subject_hash: "verified-subject".into(),
            display_identity: "person@example.invalid".into(),
        },
        1,
    )
    .expect("OAuth bundle")
}

// MUTATION CHECK (W5a provider dispatch): remove the `openai` arm from
// `build_account_provider` (restoring the old Anthropic-only builder).
// Expected failure: the OpenAI resolution below returns InvalidArgument
// before its capability document can identify the native adapter.
#[tokio::test]
async fn production_account_factory_dispatches_openai_and_preserves_anthropic() {
    let validator = ProviderCredentialValidator;
    assert!(validator.supports(ANTHROPIC_PROVIDER_NAME));
    assert!(validator.supports(OPENAI_PROVIDER_NAME));
    assert!(
        !validator.supports(OPENAI_COMPATIBLE_PROVIDER_NAME),
        "W5c must first carry base_url into compatible login validation"
    );

    let vault = Arc::new(MemoryVault::default());
    let anthropic_alias = CredentialAlias::new("anthropic-dispatch");
    let openai_alias = CredentialAlias::new("openai-dispatch");
    let compatible_alias = CredentialAlias::new("compatible-dispatch");
    vault
        .put(&anthropic_alias, b"anthropic-fixture-secret")
        .unwrap_or_else(|error| panic!("{error:?}"));
    vault
        .put(&openai_alias, b"openai-fixture-secret")
        .unwrap_or_else(|error| panic!("{error:?}"));
    vault
        .put(&compatible_alias, b"compatible-fixture-secret")
        .unwrap_or_else(|error| panic!("{error:?}"));
    let snapshot = Arc::new(StdMutex::new(vec![
        CredentialDescriptor {
            alias: anthropic_alias.clone(),
            provider: ANTHROPIC_PROVIDER_NAME.into(),
            base_url: None,
            auth_method: AuthMethod::ApiKey,
            identity: "anthropic fixture".into(),
            status: CredentialStatus::Ok,
            active: true,
        },
        CredentialDescriptor {
            alias: openai_alias.clone(),
            provider: OPENAI_PROVIDER_NAME.into(),
            base_url: None,
            auth_method: AuthMethod::ApiKey,
            identity: "openai fixture".into(),
            status: CredentialStatus::Ok,
            active: true,
        },
        CredentialDescriptor {
            alias: compatible_alias.clone(),
            provider: OPENAI_COMPATIBLE_PROVIDER_NAME.into(),
            base_url: Some("http://127.0.0.1:11434".into()),
            auth_method: AuthMethod::ApiKey,
            identity: "compatible fixture".into(),
            status: CredentialStatus::Ok,
            active: true,
        },
    ]));
    let factory = AccountsProviderFactory::new(
        snapshot,
        VaultProvision::Available(vault as Arc<dyn Vault>),
        Arc::new(ProductionAccountBuilder),
    );
    let metadata = |provider: &str, model: &str| haider_protocol::session::SessionMetadataV1 {
        cwd: "/tmp/haider-provider-dispatch".into(),
        provider: provider.into(),
        model: model.into(),
        max_tokens: 64,
        system_prompt_version: None,
        created_at_ms: 1,
    };

    let openai = factory
        .resolve_for_turn(&metadata(OPENAI_PROVIDER_NAME, "gpt-5-test"))
        .await
        .unwrap_or_else(|error| panic!("openai dispatch: {error:?}"));
    let anthropic = factory
        .resolve_for_turn(&metadata(ANTHROPIC_PROVIDER_NAME, "claude-test"))
        .await
        .unwrap_or_else(|error| panic!("anthropic dispatch: {error:?}"));
    let compatible = factory
        .resolve_for_turn(&metadata(OPENAI_COMPATIBLE_PROVIDER_NAME, "llama-test"))
        .await
        .unwrap_or_else(|error| panic!("compatible dispatch: {error:?}"));

    assert_eq!(
        openai.provider.capabilities().await.provider,
        OPENAI_PROVIDER_NAME
    );
    assert_eq!(openai.account_alias.as_deref(), Some(openai_alias.as_str()));
    assert_eq!(
        anthropic.provider.capabilities().await.provider,
        ANTHROPIC_PROVIDER_NAME
    );
    assert_eq!(
        anthropic.account_alias.as_deref(),
        Some(anthropic_alias.as_str())
    );
    assert_eq!(
        compatible.provider_name, OPENAI_COMPATIBLE_PROVIDER_NAME,
        "successful resolution pins the compatible dispatch arm"
    );
    assert_eq!(
        compatible.account_alias.as_deref(),
        Some(compatible_alias.as_str())
    );
}

/// MUTATION CHECK (review of record, W5b retrospective): weaken any component
/// of the generation/issuer/audience/resource/subject-hash fence in
/// `apply_oauth_refresh`, `expire_oauth_refresh`, or `begin_oauth_refresh`.
/// Expected failure: the late apply overwrites the replacement bundle in the
/// vault, or a stale-fence expiry/begin reports `true` and marks the
/// replacement `Expired`.
///
/// This pins the PRODUCTION account actor. The `late_refresh_*` tests in
/// `oauth_tests.rs` drive `start_status_actor`, a test double that
/// reimplements this fence, so they do not cover these functions at all —
/// the entire CAS could be deleted at all three sites with the daemon suite
/// still green.
/// Verified by revert on 2026-07-29.
#[tokio::test]
async fn stale_oauth_fences_cannot_overwrite_or_expire_a_replaced_bundle() {
    let bundle_at = |generation: u64, access: &[u8]| {
        haider_accounts::OAuthTokenBundleV1::new(
            "fake-oauth".into(),
            "http://127.0.0.1:32109".into(),
            "fake-audience".into(),
            Some("fake-api-resource".into()),
            "Bearer".into(),
            Zeroizing::new(access.to_vec()),
            Some(Zeroizing::new(
                b"OAUTH_REFRESH_FENCE_SENTINEL_4b7e".to_vec(),
            )),
            u64::MAX - 1,
            Some(u64::MAX),
            vec!["openid".into(), "inference".into()],
            haider_accounts::OAuthIdentityV1 {
                subject_hash: "verified-subject".into(),
                display_identity: "person@example.invalid".into(),
            },
            generation,
        )
        .expect("OAuth bundle")
    };
    let fence_for = |bundle: &haider_accounts::OAuthTokenBundleV1| OAuthRefreshFence {
        fence_epoch: 0,
        generation: bundle.generation,
        issuer: bundle.issuer.clone(),
        audience: bundle.audience.clone(),
        resource: bundle.resource.clone(),
        subject_hash: bundle.identity.subject_hash.clone(),
    };

    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let alias = CredentialAlias::new("fenced-oauth");
    let descriptor = CredentialDescriptor {
        alias: alias.clone(),
        provider: "fake-oauth".into(),
        base_url: None,
        auth_method: AuthMethod::OAuth,
        identity: "person@example.invalid".into(),
        status: CredentialStatus::Ok,
        active: true,
    };
    let mut accounts = memory_accounts();
    accounts.add(descriptor.clone()).expect("descriptor");
    let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(accounts.list().to_vec()));
    let vault = Arc::new(MemoryVault::new());

    // The vault already holds generation 2: a concurrent refresh or re-login
    // replaced the bundle while an older refresh was still in flight.
    let replacement = bundle_at(2, b"ACCESS_REPLACEMENT_SENTINEL_c0de");
    vault
        .put(&alias, &replacement.encode().expect("encode replacement"))
        .expect("seed replacement");
    let superseded = bundle_at(1, b"ACCESS_SUPERSEDED_SENTINEL_0old");
    let late = bundle_at(1, b"ACCESS_LATE_SENTINEL_dead");

    let mut actor = start_account_actor(AccountActorConfig {
        store: store.clone(),
        accounts,
        vault: vault.clone() as Arc<dyn Vault>,
        validator: Arc::new(ProviderCredentialValidator),
        snapshot: Arc::clone(&snapshot),
        management: None,
        profile_id: "oauth-fence".into(),
        default_model: "gpt-test".into(),
        providers: test_provider_registry(),
        provider_endpoint_validator: Arc::new(ProductionProviderEndpointValidator),
        reserved_aliases: HashSet::new(),
        refresh_fences: RefreshFenceRegistry::default(),
    });
    let commands = actor.commands();

    let (completed, applied) = tokio::sync::oneshot::channel();
    commands
        .send(AccountCommand::ApplyOAuthRefresh {
            descriptor: descriptor.clone(),
            expected: fence_for(&superseded),
            encoded_bundle: late.encode().expect("encode late"),
            completed,
        })
        .await
        .expect("apply command");
    assert!(matches!(
        applied.await.expect("apply response"),
        Err(RefreshApplyError::Stale)
    ));

    let stored = vault.resolve(&alias).expect("stored bundle");
    let stored =
        haider_accounts::OAuthTokenBundleV1::decode(stored.expose_secret()).expect("decode stored");
    assert_eq!(
        stored.generation, 2,
        "replacement must survive a late apply"
    );
    assert_eq!(
        stored.access_token_handle().expose_secret(),
        b"ACCESS_REPLACEMENT_SENTINEL_c0de"
    );

    let (completed, expired) = tokio::sync::oneshot::channel();
    commands
        .send(AccountCommand::ExpireOAuthRefresh {
            descriptor: descriptor.clone(),
            expected: fence_for(&superseded),
            completed,
        })
        .await
        .expect("expire command");
    assert!(
        !expired.await.expect("expire response").expect("no error"),
        "a stale fence must not expire the replacement"
    );

    let (completed, began) = tokio::sync::oneshot::channel();
    commands
        .send(AccountCommand::BeginOAuthRefresh {
            descriptor: descriptor.clone(),
            expected: fence_for(&superseded),
            completed,
        })
        .await
        .expect("begin command");
    assert!(
        !began.await.expect("begin response").expect("no error"),
        "a stale fence must not open a refresh against the replacement"
    );

    {
        let current = snapshot.lock().expect("snapshot");
        assert!(
            current
                .iter()
                .any(|entry| entry.alias == alias && entry.status == CredentialStatus::Ok),
            "the replacement descriptor must remain usable"
        );
    }

    actor.shutdown().await;
    store.close().await.expect("close");
}

/// MUTATION CHECK (review of record, W5c.1): drop the `error.retryable` arm
/// from the attempt resolver's rotation-failure match so the resolver error
/// propagates. Expected failure: the retryable bookkeeping error escapes as
/// `Err`, the decision is never `Wait`, and the turn dies on an error the
/// provider itself said to retry.
/// Verified by revert on 2026-07-29.
#[tokio::test]
async fn retryable_rotation_bookkeeping_failure_waits_instead_of_killing_the_turn() {
    struct WriteRefusingStore(Vec<CredentialDescriptor>);
    impl StoreLike for WriteRefusingStore {
        fn load(&self) -> Result<Vec<CredentialDescriptor>, HaiderError> {
            Ok(self.0.clone())
        }
        fn save(&self, _descriptors: &[CredentialDescriptor]) -> Result<(), HaiderError> {
            Err(HaiderError::new(
                ErrorCode::StoreLocked,
                "descriptor store is temporarily locked",
                true,
            ))
        }
    }

    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let primary_alias = CredentialAlias::new("wedged-primary");
    let alternate_alias = CredentialAlias::new("wedged-backup");
    let descriptor = |alias: CredentialAlias, active: bool| CredentialDescriptor {
        alias,
        provider: OPENAI_PROVIDER_NAME.into(),
        base_url: None,
        auth_method: AuthMethod::ApiKey,
        identity: "wedged fixture".into(),
        status: CredentialStatus::Ok,
        active,
    };
    let descriptors = vec![
        descriptor(primary_alias.clone(), true),
        descriptor(alternate_alias.clone(), false),
    ];
    let backing: Box<dyn StoreLike> = Box::new(WriteRefusingStore(descriptors.clone()));
    let accounts = AccountStore::new(backing).expect("accounts");
    let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(descriptors));
    let vault = Arc::new(MemoryVault::new());
    vault
        .put(&alternate_alias, b"wedged-alternate-secret")
        .expect("alternate secret");
    let mut actor = start_account_actor(AccountActorConfig {
        store: store.clone(),
        accounts,
        vault: vault.clone() as Arc<dyn Vault>,
        validator: Arc::new(ProviderCredentialValidator),
        snapshot: Arc::clone(&snapshot),
        management: None,
        profile_id: "wedged-store".into(),
        default_model: "gpt-test".into(),
        providers: test_provider_registry(),
        provider_endpoint_validator: Arc::new(ProductionProviderEndpointValidator),
        reserved_aliases: HashSet::new(),
        refresh_fences: RefreshFenceRegistry::default(),
    });
    let broker = CredentialBroker::new(
        vault.clone() as Arc<dyn Vault>,
        OAuthProviderCatalog::default(),
        Arc::clone(&snapshot),
        actor.commands(),
    )
    .expect("broker");
    let factory = AccountsProviderFactory::with_broker(
        Arc::clone(&snapshot),
        VaultProvision::Available(vault as Arc<dyn Vault>),
        Arc::new(ProductionAccountBuilder),
        broker.clone(),
    );
    let resolver = AccountsAttemptResolver::new(
        factory,
        haider_protocol::session::SessionMetadataV1 {
            cwd: "/tmp/wedged-store".into(),
            provider: OPENAI_PROVIDER_NAME.into(),
            model: "gpt-test".into(),
            max_tokens: 64,
            system_prompt_version: None,
            created_at_ms: 1,
        },
    );

    let decision = resolver
        .resolve(
            &primary_alias,
            &haider_provider::ProviderError::new(
                haider_provider::ProviderErrorKind::RateLimited,
                "bounded rate limit",
            )
            .with_retry_after_ms(Some(1_000)),
        )
        .await
        .expect("a retryable bookkeeping failure is not a turn-fatal error");
    assert!(matches!(
        decision,
        haider_core::ProviderAttemptDecision::Wait
    ));

    assert!(broker.shutdown().await);
    actor.shutdown().await;
    store.close().await.expect("close");
}

/// MUTATION CHECK (W5c.1 resolver-backed factory): bypass
/// `CredentialBroker::resolve_account` and restore direct active-snapshot
/// resolution. Expected failure: the limited active alias is rejected or
/// returned instead of the checked one-hop alternate, and no durable
/// selection/rotation metadata is visible.
/// Verified by revert (direct bypass and fabricated auth deadline) on
/// 2026-07-29.
#[tokio::test]
async fn factory_uses_checked_resolver_and_durably_selects_one_limited_alternate() {
    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let mut accounts = memory_accounts();
    let limited_alias = CredentialAlias::new("limited-primary");
    let alternate_alias = CredentialAlias::new("usable-backup");
    let refresh_failed_alias = CredentialAlias::new("refresh-failed-primary");
    let refresh_backup_alias = CredentialAlias::new("refresh-usable-backup");
    accounts
        .add(CredentialDescriptor {
            alias: limited_alias.clone(),
            provider: OPENAI_PROVIDER_NAME.into(),
            base_url: None,
            auth_method: AuthMethod::ApiKey,
            identity: "limited fixture".into(),
            status: CredentialStatus::Limited { until_ms: u64::MAX },
            active: true,
        })
        .expect("limited descriptor");
    accounts
        .add(CredentialDescriptor {
            alias: alternate_alias.clone(),
            provider: OPENAI_PROVIDER_NAME.into(),
            base_url: None,
            auth_method: AuthMethod::ApiKey,
            identity: "backup fixture".into(),
            status: CredentialStatus::Ok,
            active: false,
        })
        .expect("alternate descriptor");
    accounts
        .add(CredentialDescriptor {
            alias: refresh_failed_alias.clone(),
            provider: ANTHROPIC_PROVIDER_NAME.into(),
            base_url: None,
            auth_method: AuthMethod::OAuth,
            identity: "refresh failed fixture".into(),
            status: CredentialStatus::Ok,
            active: true,
        })
        .expect("refresh-failed descriptor");
    accounts
        .add(CredentialDescriptor {
            alias: refresh_backup_alias.clone(),
            provider: ANTHROPIC_PROVIDER_NAME.into(),
            base_url: None,
            auth_method: AuthMethod::ApiKey,
            identity: "refresh backup fixture".into(),
            status: CredentialStatus::Ok,
            active: false,
        })
        .expect("refresh backup descriptor");
    let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(accounts.list().to_vec()));
    let vault = Arc::new(MemoryVault::new());
    vault
        .put(&alternate_alias, b"checked-alternate-secret")
        .expect("alternate vault secret");
    let mut actor = start_account_actor(AccountActorConfig {
        store: store.clone(),
        accounts,
        vault: vault.clone() as Arc<dyn Vault>,
        validator: Arc::new(ProviderCredentialValidator),
        snapshot: Arc::clone(&snapshot),
        management: None,
        profile_id: "resolver-factory".into(),
        default_model: "gpt-test".into(),
        providers: test_provider_registry(),
        provider_endpoint_validator: Arc::new(ProductionProviderEndpointValidator),
        reserved_aliases: HashSet::new(),
        refresh_fences: RefreshFenceRegistry::default(),
    });
    let broker = CredentialBroker::new(
        vault.clone() as Arc<dyn Vault>,
        OAuthProviderCatalog::default(),
        Arc::clone(&snapshot),
        actor.commands(),
    )
    .expect("broker");
    let factory = AccountsProviderFactory::with_broker(
        Arc::clone(&snapshot),
        VaultProvision::Available(vault as Arc<dyn Vault>),
        Arc::new(ProductionAccountBuilder),
        broker.clone(),
    );
    let resolved = factory
        .resolve_for_turn(&haider_protocol::session::SessionMetadataV1 {
            cwd: "/tmp/resolver-factory".into(),
            provider: OPENAI_PROVIDER_NAME.into(),
            model: "gpt-test".into(),
            max_tokens: 64,
            system_prompt_version: None,
            created_at_ms: 1,
        })
        .await
        .expect("checked alternate resolution");

    assert_eq!(
        resolved.account_alias.as_deref(),
        Some(alternate_alias.as_str())
    );
    assert!(resolved.rotation_budget_consumed);
    assert_eq!(
        resolved.initial_rotation,
        Some(haider_protocol::credential::RotationEvent {
            provider: OPENAI_PROVIDER_NAME.into(),
            from: limited_alias.clone(),
            to: alternate_alias.clone(),
            cause: haider_protocol::credential::RotationCause::RateLimit,
        })
    );
    let refresh_rotation = broker
        .resolve_account(
            ANTHROPIC_PROVIDER_NAME,
            Some((refresh_failed_alias.clone(), RotationTrigger::RefreshFailed)),
        )
        .await
        .expect("refresh failure uses checked alternate");
    assert_eq!(
        refresh_rotation.rotation,
        Some(haider_protocol::credential::RotationEvent {
            provider: ANTHROPIC_PROVIDER_NAME.into(),
            from: refresh_failed_alias.clone(),
            to: refresh_backup_alias.clone(),
            cause: haider_protocol::credential::RotationCause::Error,
        })
    );
    {
        let current = snapshot.lock().expect("snapshot");
        assert!(
            current
                .iter()
                .any(|descriptor| descriptor.alias == alternate_alias && descriptor.active)
        );
        assert!(
            current
                .iter()
                .any(|descriptor| descriptor.alias == limited_alias && !descriptor.active)
        );
        assert!(current.iter().any(|descriptor| {
            descriptor.alias == refresh_failed_alias
                && matches!(descriptor.status, CredentialStatus::Expired)
                && !descriptor.active
        }));
        assert!(current.iter().any(|descriptor| {
            descriptor.alias == refresh_backup_alias
                && descriptor.status == CredentialStatus::Ok
                && descriptor.active
        }));
    }
    drop(factory);
    assert!(broker.shutdown().await);
    actor.shutdown().await;
    store.close().await.expect("close");
}

/// MUTATION CHECK: dispatch either OAuth descriptor through its API-key arm,
/// remove the sanctioned-provider match, or pass the encoded bundle to an
/// adapter. The broker/factory resolution below fails before capabilities.
/// Verified by revert on 2026-07-29.
#[tokio::test]
async fn auth_aware_factory_routes_sanctioned_oauth_descriptors_to_subscription_adapters() {
    let vault = Arc::new(MemoryVault::default());
    let openai_alias = CredentialAlias::new("openai-oauth-dispatch");
    let anthropic_alias = CredentialAlias::new("anthropic-oauth-dispatch");
    let descriptor = |alias: CredentialAlias, provider: &str| CredentialDescriptor {
        alias,
        provider: provider.into(),
        base_url: None,
        auth_method: AuthMethod::OAuth,
        identity: format!("{provider} fixture"),
        status: CredentialStatus::Ok,
        active: true,
    };
    let openai_descriptor = descriptor(openai_alias.clone(), OPENAI_OAUTH_PROVIDER_NAME);
    let anthropic_descriptor = descriptor(anthropic_alias.clone(), ANTHROPIC_OAUTH_PROVIDER_NAME);
    let bundle = |provider: &str, issuer: &str, audience: &str, scopes: &[&str], access: &[u8]| {
        haider_accounts::OAuthTokenBundleV1::new(
            provider.into(),
            issuer.into(),
            audience.into(),
            None,
            "Bearer".into(),
            Zeroizing::new(access.to_vec()),
            Some(Zeroizing::new(b"REFRESH_FACTORY_SENTINEL_f0b1".to_vec())),
            u64::MAX - 2,
            Some(u64::MAX - 1),
            scopes.iter().map(|scope| (*scope).to_owned()).collect(),
            haider_accounts::OAuthIdentityV1 {
                subject_hash: format!("{provider}-subject"),
                display_identity: format!("{provider} subscription"),
            },
            1,
        )
        .expect("OAuth bundle")
    };
    vault
        .put(
            &openai_alias,
            &bundle(
                OPENAI_OAUTH_PROVIDER_NAME,
                "https://auth.openai.com",
                "app_EMoamEEZ73f0CkXaXp7hrann",
                &["openid", "profile", "email", "offline_access"],
                b"OPENAI_FACTORY_ACCESS_SENTINEL_18a4",
            )
            .encode()
            .expect("encode OpenAI bundle"),
        )
        .expect("store OpenAI bundle");
    vault
        .put(
            &anthropic_alias,
            &bundle(
                ANTHROPIC_OAUTH_PROVIDER_NAME,
                "https://claude.ai",
                "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
                &["user:inference"],
                b"ANTHROPIC_FACTORY_ACCESS_SENTINEL_d716",
            )
            .encode()
            .expect("encode Anthropic bundle"),
        )
        .expect("store Anthropic bundle");
    let snapshot = Arc::new(StdMutex::new(vec![
        openai_descriptor.clone(),
        anthropic_descriptor.clone(),
    ]));
    let (status_commands, mut status_receiver) = mpsc::channel(4);
    let resolver_snapshot = Arc::clone(&snapshot);
    let status_task = tokio::spawn(async move {
        while let Some(command) = status_receiver.recv().await {
            if let AccountCommand::ResolveCredential {
                provider,
                failure: None,
                completed,
            } = command
            {
                let result = resolver_snapshot
                    .lock()
                    .ok()
                    .and_then(|descriptors| {
                        descriptors
                            .iter()
                            .find(|descriptor| descriptor.provider == provider && descriptor.active)
                            .cloned()
                    })
                    .map(|descriptor| ResolvedAccount {
                        descriptor,
                        rotation: None,
                    })
                    .ok_or_else(|| {
                        HaiderError::new(
                            ErrorCode::CredentialMissing,
                            "test resolver has no active descriptor",
                            false,
                        )
                    });
                let _ = completed.send(result);
            }
        }
    });
    let broker = CredentialBroker::new(
        vault.clone() as Arc<dyn Vault>,
        OAuthProviderCatalog::default(),
        Arc::clone(&snapshot),
        status_commands,
    )
    .expect("credential broker");
    let factory = AccountsProviderFactory::with_broker(
        snapshot,
        VaultProvision::Available(vault.clone() as Arc<dyn Vault>),
        Arc::new(ProductionAccountBuilder),
        broker,
    );
    let metadata = |provider: &str, model: &str| haider_protocol::session::SessionMetadataV1 {
        cwd: "/tmp/haider-oauth-dispatch".into(),
        provider: provider.into(),
        model: model.into(),
        max_tokens: 64,
        system_prompt_version: None,
        created_at_ms: 1,
    };
    let openai = factory
        .resolve_for_turn(&metadata(OPENAI_OAUTH_PROVIDER_NAME, "gpt-oauth"))
        .await
        .expect("OpenAI OAuth dispatch");
    let anthropic = factory
        .resolve_for_turn(&metadata(ANTHROPIC_OAUTH_PROVIDER_NAME, "claude-oauth"))
        .await
        .expect("Anthropic OAuth dispatch");
    assert_eq!(openai.provider_name, OPENAI_OAUTH_PROVIDER_NAME);
    assert_eq!(
        openai.provider.credential_surface(),
        haider_provider::ProviderCredentialSurface::OAuthSubscriptionBearer
    );
    assert_eq!(
        openai.provider.capabilities().await.provider,
        OPENAI_PROVIDER_NAME
    );
    assert_eq!(anthropic.provider_name, ANTHROPIC_OAUTH_PROVIDER_NAME);
    assert_eq!(
        anthropic.provider.credential_surface(),
        haider_provider::ProviderCredentialSurface::OAuthSubscriptionBearer
    );
    assert_eq!(
        anthropic.provider.capabilities().await.provider,
        ANTHROPIC_PROVIDER_NAME
    );

    let wrong_alias = CredentialAlias::new("oauth-api-key-crosswire");
    vault
        .put(&wrong_alias, b"NEVER_CROSSWIRE_API_KEY_91f0")
        .expect("store crosswire key");
    let result = build_account_provider(
        OPENAI_OAUTH_PROVIDER_NAME,
        None,
        AuthMethod::ApiKey,
        vault.resolve(&wrong_alias).expect("resolve crosswire key"),
        "gpt-oauth",
        &wrong_alias,
    );
    let Err(error) = result else {
        panic!("OAuth provider ID must reject API-key mode");
    };
    assert_eq!(error.code, ErrorCode::InvalidArgument);
    drop((openai, anthropic, factory));
    status_task.await.expect("status task");
}

// MUTATION CHECK (R10 Keychain namespacing): remove the profile input from
// `physical_alias` (hash provider+command only). Expected failure: the two
// profiles below derive the SAME physical alias and the distinct-secret
// assertion fails.
#[test]
fn identical_display_aliases_in_two_profiles_resolve_distinct_secrets() {
    let a = physical_alias("profile-a", "anthropic", "command-1");
    let b = physical_alias("profile-b", "anthropic", "command-1");
    assert_ne!(a, b, "physical aliases must be profile-namespaced");
    assert_eq!(
        physical_alias("profile-a", "anthropic", "command-1"),
        a,
        "physical alias derivation must be stable for retries"
    );

    // Same display alias "work", two profiles, one shared vault backend:
    // the physical namespacing keeps the secrets distinct.
    let vault = MemoryVault::default();
    let alias_a = CredentialAlias::new(a);
    let alias_b = CredentialAlias::new(b);
    vault
        .put(&alias_a, b"secret-for-profile-a")
        .unwrap_or_else(|error| panic!("{error:?}"));
    vault
        .put(&alias_b, b"secret-for-profile-b")
        .unwrap_or_else(|error| panic!("{error:?}"));
    let resolved_a = vault
        .resolve(&alias_a)
        .unwrap_or_else(|error| panic!("{error:?}"));
    let resolved_b = vault
        .resolve(&alias_b)
        .unwrap_or_else(|error| panic!("{error:?}"));
    assert_eq!(resolved_a.expose_secret(), b"secret-for-profile-a");
    assert_eq!(resolved_b.expose_secret(), b"secret-for-profile-b");
}

#[test]
fn canonical_login_identity_excludes_vault_reference_and_secret_material() {
    let identity = identity_for("profile-a", "command-1");
    let json = identity
        .canonical_json()
        .unwrap_or_else(|error| panic!("{error:?}"));
    assert!(json.contains("anthropic") && json.contains("claude-test"));
    assert!(
        !json.contains("vaultref") && !json.contains("secret"),
        "command identity must exclude the ephemeral reference and secrets: {json}"
    );
    // Round-trips for the reconciliation reader.
    let decoded: LoginIdentity =
        serde_json::from_str(&json).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(decoded, identity);
}

#[test]
fn staged_secrets_dedupe_by_stage_id_and_claims_are_single_use() {
    let mut stages = StagedSecrets::default();
    let (reference, expires) = stages
        .stage("stage-1", StagePurpose::ApiKey, b"sk-first")
        .unwrap_or_else(|_| panic!("stage"));
    assert!(expires > 0);
    // Same id + same bytes: the SAME reference (same-connection retry).
    let (again, _) = stages
        .stage("stage-1", StagePurpose::ApiKey, b"sk-first")
        .unwrap_or_else(|_| panic!("re-stage"));
    assert_eq!(reference, again);
    // Same id + different bytes: invalid.
    assert!(matches!(
        stages.stage("stage-1", StagePurpose::ApiKey, b"sk-other"),
        Err(StageError::Mismatch)
    ));
    // Claim is single-use.
    let (purpose, secret) = stages
        .claim(&reference)
        .unwrap_or_else(|| panic!("first claim"));
    assert_eq!(purpose, StagePurpose::ApiKey);
    assert_eq!(secret.as_slice(), b"sk-first");
    assert!(
        stages.claim(&reference).is_none(),
        "references are single-use"
    );
    // Unknown references never claim.
    assert!(stages.claim("vaultref-doesnotexist").is_none());
}

// MUTATION CHECK (R7 stage TTL): remove `sweep_expired` from `claim`.
// Expected failure: the expired reference below still claims.
#[tokio::test(start_paused = true)]
async fn staged_secrets_expire_after_the_five_minute_ttl() {
    let mut stages = StagedSecrets::default();
    let (reference, _) = stages
        .stage("stage-1", StagePurpose::ApiKey, b"sk-expiring")
        .unwrap_or_else(|_| panic!("stage"));
    tokio::time::advance(SECRET_TTL + Duration::from_secs(1)).await;
    assert!(
        stages.claim(&reference).is_none(),
        "an expired stage must be wiped, forcing restage_required"
    );
}

async fn open_store(dir: &std::path::Path) -> SqliteStoreHandle {
    SqliteStoreHandle::open(dir)
        .await
        .unwrap_or_else(|error| panic!("open store: {}", error.message))
}

// ─────────────────── pending-command secret TTL (R7/R10) ────────────────────

/// Correlated responses land in an unbounded channel (the account actor
/// never awaits delivery, so the sink must never block).
struct ChannelSink(mpsc::UnboundedSender<WireFrame>);

impl FrameSink for ChannelSink {
    fn try_send(&self, frame: WireFrame) -> Result<(), FrameSendError> {
        self.0.send(frame).map_err(|_| FrameSendError)
    }
}

fn channel_sink() -> (Arc<dyn FrameSink>, mpsc::UnboundedReceiver<WireFrame>) {
    let (sender, receiver) = mpsc::unbounded_channel();
    (Arc::new(ChannelSink(sender)), receiver)
}

/// Always reports the R10 "validation unavailable" arm — the retryable
/// failure that leaves the secret with the COMMAND — and counts its calls,
/// so a reused expired secret is visible as a second validation.
#[derive(Default)]
struct UnavailableValidator {
    calls: std::sync::atomic::AtomicUsize,
}

impl UnavailableValidator {
    fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl CredentialValidator for UnavailableValidator {
    fn supports(&self, provider: &str) -> bool {
        provider == "anthropic"
    }

    async fn validate(
        &self,
        _provider: &str,
        _model: &str,
        _secret: &[u8],
    ) -> Result<ValidatedIdentity, ValidationError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(ValidationError {
            kind: ValidationFailureKind::Unavailable,
            message: "credential validation reported Overloaded".into(),
        })
    }
}

fn login_job(
    command_id: &str,
    request_id: &str,
    secret: Option<&[u8]>,
    sink: &Arc<dyn FrameSink>,
) -> LoginJob {
    LoginJob {
        command_id: command_id.to_owned(),
        provider: "anthropic".into(),
        display_alias: Some("work".into()),
        validation_model: Some("claude-test".into()),
        secret: secret.map(|bytes| Zeroizing::new(bytes.to_vec())),
        route: LoginRoute {
            request_id: RequestId::new(request_id),
            sink: Arc::clone(sink),
        },
    }
}

fn expect_error(frame: WireFrame) -> (String, bool) {
    match frame {
        WireFrame::Response {
            body: ResponseBody::Error {
                code, retryable, ..
            },
            ..
        } => (code, retryable),
        other => panic!("expected a correlated error, got {other:?}"),
    }
}

/// A failed receipt/revision commit cannot publish externally mutated account
/// state under the prior management revision.
///
/// MUTATION CHECK: move `publish_management_snapshot` above
/// `finalize_login_receipt` in `finalize_and_respond`. Expected runtime
/// failure: after the injected store close, the descriptor appears in both
/// published snapshots even though the response is an error and revision is
/// still zero.
#[tokio::test]
async fn login_publishes_only_after_receipt_and_revision_commit() {
    struct HeldSuccessValidator {
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl CredentialValidator for HeldSuccessValidator {
        fn supports(&self, provider: &str) -> bool {
            provider == "anthropic"
        }

        async fn validate(
            &self,
            _provider: &str,
            _model: &str,
            _secret: &[u8],
        ) -> Result<ValidatedIdentity, ValidationError> {
            self.entered.notify_one();
            self.release.notified().await;
            Ok(ValidatedIdentity {
                identity: "held success".into(),
            })
        }
    }

    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(Vec::new()));
    let management = ManagementSnapshot::new(0, Vec::new(), Vec::new());
    let vault = Arc::new(MemoryVault::new());
    let mut actor = start_account_actor(AccountActorConfig {
        store: store.clone(),
        accounts: memory_accounts(),
        vault: vault.clone() as Arc<dyn Vault>,
        validator: Arc::new(HeldSuccessValidator {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        }),
        snapshot: Arc::clone(&snapshot),
        management: Some(management.clone()),
        profile_id: "publish-order".into(),
        default_model: "claude-test".into(),
        providers: test_provider_registry(),
        provider_endpoint_validator: Arc::new(ProductionProviderEndpointValidator),
        reserved_aliases: HashSet::new(),
        refresh_fences: RefreshFenceRegistry::default(),
    });
    let (sink, mut frames) = channel_sink();
    actor
        .commands()
        .send(AccountCommand::Login(Box::new(login_job(
            "publish-order-command",
            "publish-order-request",
            Some(b"publish-order-secret"),
            &sink,
        ))))
        .await
        .expect("send login");

    entered.notified().await;
    store.clone().close().await.expect("close receipt store");
    release.notify_one();
    let frame = tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("response deadline")
        .expect("response");
    let (code, retryable) = expect_error(frame);
    assert_eq!(code, ERROR_CODE_PROVIDER_ERROR);
    assert!(retryable);
    assert!(
        !vault.list().expect("vault list").is_empty(),
        "external vault mutation must have succeeded before finalization failed"
    );
    assert!(snapshot.lock().expect("resolver snapshot").is_empty());
    let view = management.read().expect("management snapshot");
    assert_eq!(view.revision, 0);
    assert!(view.descriptors.is_empty());

    actor.shutdown().await;
}

// MUTATION CHECK (R7/R10 pending-command secret TTL): disable BOTH
// enforcement sites — `handle_login`'s claim guard
// `Some(entry) if entry.claimed_at.elapsed() < SECRET_TTL` and the actor
// loop's `pending.retain(|_, entry| entry.claimed_at.elapsed() < SECRET_TTL)`
// — by replacing each condition with `true`. Expected failure: the expired
// secret is reused, so the stage-less retry validates a SECOND time and
// answers `provider_error` instead of `restage_required`, the pending map is
// not empty at the assertion, and the validator call count is 2.
#[tokio::test(start_paused = true)]
async fn pending_login_secret_past_the_ttl_is_wiped_and_forces_restage() {
    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let mut accounts = memory_accounts();
    let vault = MemoryVault::default();
    let validator = UnavailableValidator::default();
    let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(Vec::new()));
    let mut pending: HashMap<String, PendingSecret> = HashMap::new();
    let (sink, mut frames) = channel_sink();

    // Retryable validation failure: the COMMAND keeps the claimed secret so
    // the same command can retry without retyping the key.
    handle_login(
        &store,
        &mut accounts,
        &vault,
        &validator,
        &snapshot,
        None,
        "profile-ttl",
        "claude-test",
        &mut pending,
        &HashSet::new(),
        login_job("command-ttl", "req-1", Some(b"sk-retained"), &sink),
    )
    .await;
    let (code, retryable) = expect_error(
        frames
            .try_recv()
            .unwrap_or_else(|error| panic!("first login response: {error}")),
    );
    assert_eq!(code, ERROR_CODE_PROVIDER_ERROR);
    assert!(retryable, "validation unavailability must be retryable");
    assert!(
        pending.contains_key("command-ttl"),
        "a retryable validation must retain the command-owned secret"
    );
    assert_eq!(validator.calls(), 1);

    tokio::time::advance(SECRET_TTL + Duration::from_secs(1)).await;

    // Stage-less retry of the SAME command, now past the TTL.
    handle_login(
        &store,
        &mut accounts,
        &vault,
        &validator,
        &snapshot,
        None,
        "profile-ttl",
        "claude-test",
        &mut pending,
        &HashSet::new(),
        login_job("command-ttl", "req-2", None, &sink),
    )
    .await;
    let (code, retryable) = expect_error(
        frames
            .try_recv()
            .unwrap_or_else(|error| panic!("retry response: {error}")),
    );
    // The WIRE literal, not just the constant (see the haider-rpc golden
    // `account_and_vault_stable_codes_pin_their_wire_literals`).
    assert_eq!(
        code, "restage_required",
        "an expired command-owned secret must force an explicit restage"
    );
    assert_eq!(code, ERROR_CODE_RESTAGE_REQUIRED);
    assert!(retryable, "restage_required is retryable once re-staged");

    // WIPED, not merely unreachable: the entry is gone from the pending map,
    // dropping (and zeroizing) the secret with it.
    assert!(
        pending.is_empty(),
        "an expired pending secret must be wiped, not parked"
    );
    assert_eq!(
        validator.calls(),
        1,
        "an expired secret must never reach the validator again"
    );

    // Neither leg persisted anything.
    assert!(
        vault
            .list()
            .unwrap_or_else(|error| panic!("{error:?}"))
            .is_empty(),
        "no vault entries"
    );
    assert!(accounts.list().is_empty(), "no descriptors");

    store
        .close()
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
}

// MUTATION CHECK (same two TTL sites, driven through the ACTOR loop): with
// both conditions replaced by `true` the expired secret survives the actor's
// pre-command sweep and is handed to validation again, so the second
// response below is `provider_error`, not `restage_required`.
#[tokio::test(start_paused = true)]
async fn account_actor_answers_restage_required_after_the_pending_ttl() {
    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let validator = Arc::new(UnavailableValidator::default());
    let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(Vec::new()));
    let mut actor = start_account_actor(AccountActorConfig {
        store: store.clone(),
        accounts: memory_accounts(),
        vault: Arc::new(MemoryVault::default()) as Arc<dyn Vault>,
        validator: Arc::clone(&validator) as Arc<dyn CredentialValidator>,
        snapshot,
        management: None,
        profile_id: "profile-ttl-actor".into(),
        default_model: "claude-test".into(),
        providers: test_provider_registry(),
        provider_endpoint_validator: Arc::new(ProductionProviderEndpointValidator),
        reserved_aliases: HashSet::new(),
        refresh_fences: RefreshFenceRegistry::default(),
    });
    let commands = actor.commands();
    let (sink, mut frames) = channel_sink();

    commands
        .send(AccountCommand::Login(Box::new(login_job(
            "command-actor-ttl",
            "req-actor-1",
            Some(b"sk-retained"),
            &sink,
        ))))
        .await
        .unwrap_or_else(|_| panic!("actor admits the first login"));
    let (code, retryable) = expect_error(
        frames
            .recv()
            .await
            .unwrap_or_else(|| panic!("first login response")),
    );
    assert_eq!(code, ERROR_CODE_PROVIDER_ERROR);
    assert!(retryable);

    tokio::time::advance(SECRET_TTL + Duration::from_secs(1)).await;

    commands
        .send(AccountCommand::Login(Box::new(login_job(
            "command-actor-ttl",
            "req-actor-2",
            None,
            &sink,
        ))))
        .await
        .unwrap_or_else(|_| panic!("actor admits the stage-less retry"));
    let (code, retryable) = expect_error(
        frames
            .recv()
            .await
            .unwrap_or_else(|| panic!("retry response")),
    );
    assert_eq!(
        code, "restage_required",
        "the actor must not hand an expired secret back to validation"
    );
    assert!(retryable);
    assert_eq!(
        validator.calls(),
        1,
        "an expired secret must never reach the validator again"
    );

    actor.shutdown().await;
    store
        .close()
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
}

/// Receipt-shape laws for the login command (the first non-wire receipt
/// caller): fresh claim, pending resume, committed replay, digest mismatch,
/// failed-terminal replay, and the reconciliation scan.
#[tokio::test]
async fn login_receipt_claims_replay_and_reject_like_every_r2_command() {
    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let identity = identity_for("profile-a", "command-1");
    let json = identity
        .canonical_json()
        .unwrap_or_else(|error| panic!("{error:?}"));
    let digest = blake3::hash(json.as_bytes()).to_hex().to_string();

    // Fresh claim, then pending resume on re-claim.
    let claim = store
        .login_claim_receipt("command-1".into(), digest.clone(), json.clone())
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
    assert_eq!(claim, LoginClaim::Fresh);
    let claim = store
        .login_claim_receipt("command-1".into(), digest.clone(), json.clone())
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
    assert_eq!(claim, LoginClaim::ResumePending);

    // Same command with a DIFFERENT semantic body is rejected (R7: changing
    // provider/resolved-model/alias under a reused command id).
    let other = LoginIdentity {
        provider: "anthropic".into(),
        resolved_model: "claude-other".into(),
        display_alias: None,
        physical_alias: identity.physical_alias.clone(),
    };
    let other_json = other
        .canonical_json()
        .unwrap_or_else(|error| panic!("{error:?}"));
    let other_digest = blake3::hash(other_json.as_bytes()).to_hex().to_string();
    let mismatch = store
        .login_claim_receipt("command-1".into(), other_digest, other_json)
        .await
        .expect_err("different body under a reused command id must be rejected");
    assert!(mismatch.message.contains("different method or semantic"));

    // The scan sees the pending row.
    let rows = store
        .login_receipts()
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, "pending");

    // Finalize commits the descriptor response; a replay returns it.
    let descriptor = CredentialDescriptor {
        alias: CredentialAlias::new(identity.physical_alias.clone()),
        provider: "anthropic".into(),
        base_url: None,
        auth_method: AuthMethod::ApiKey,
        identity: "work".into(),
        status: CredentialStatus::Ok,
        active: true,
    };
    store
        .finalize_login_receipt(
            "command-1".into(),
            LoginReceiptResponse {
                descriptor: descriptor.clone(),
            },
        )
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
    let claim = store
        .login_claim_receipt("command-1".into(), digest, json)
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
    match claim {
        LoginClaim::Committed(response) => {
            assert_eq!(response.descriptor, descriptor);
        }
        other => panic!("expected committed replay, got {other:?}"),
    }

    // A definitive failure records terminally and a replay is a typed error.
    let identity_2 = identity_for("profile-a", "command-2");
    let json_2 = identity_2
        .canonical_json()
        .unwrap_or_else(|error| panic!("{error:?}"));
    let digest_2 = blake3::hash(json_2.as_bytes()).to_hex().to_string();
    let _ = store
        .login_claim_receipt("command-2".into(), digest_2.clone(), json_2.clone())
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
    store
        .fail_login_receipt(
            "command-2".into(),
            LoginReceiptFailure {
                code: "unauthorized".into(),
                message: "credential validation reported Authentication".into(),
            },
        )
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
    let replay = store
        .login_claim_receipt("command-2".into(), digest_2, json_2)
        .await
        .expect_err("failed login receipts are terminal");
    assert!(replay.message.contains("already recorded as failed"));

    // Failed receipts are excluded from the reconciliation scan.
    let rows = store
        .login_receipts()
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
    assert_eq!(
        rows.len(),
        1,
        "only the committed receipt remains scannable"
    );
    assert_eq!(rows[0].state, "committed");

    store
        .close()
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
}

/// Pre-ready reconciliation allocates the missing final revision once after
/// durable descriptor state proves the external mutation succeeded.
///
/// MUTATION CHECK: remove the `finalize_reconciled` call from the
/// pending-plus-descriptor branch of `reconcile_login_receipts`. Expected
/// runtime failure: the first reconciliation leaves revision zero and the
/// receipt pending instead of committing revision one.
#[tokio::test]
async fn login_reconciliation_advances_a_missing_revision_exactly_once() {
    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let identity = identity_for("reconcile-revision", "reconcile-command");
    let request_json = identity.canonical_json().expect("request JSON");
    let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
    store
        .login_claim_receipt("reconcile-command".into(), request_digest, request_json)
        .await
        .expect("claim receipt");

    let alias = CredentialAlias::new(identity.physical_alias.clone());
    let mut accounts = memory_accounts();
    accounts
        .add(descriptor_for(&identity, &alias, Some("reconciled".into())))
        .expect("persist descriptor");
    let vault = VaultProvision::Available(Arc::new(MemoryVault::new()) as Arc<dyn Vault>);

    reconcile_login_receipts(&store, &mut accounts, &vault)
        .await
        .expect("first reconciliation");
    assert_eq!(store.management_revision().await.expect("revision"), 1);
    let rows = store.login_receipts().await.expect("receipt rows");
    assert_eq!(rows[0].state, "committed");
    assert_eq!(rows[0].final_revision, Some(1));

    reconcile_login_receipts(&store, &mut accounts, &vault)
        .await
        .expect("second reconciliation");
    assert_eq!(
        store.management_revision().await.expect("stable revision"),
        1
    );
}

// MUTATION CHECK (R10 step 10): make `reconcile_login_receipts` skip the
// vault-only arm (treat pending+vault as "neither"). Expected failure: the
// vault-only boundary below never produces a descriptor and its receipt
// stays pending.
#[tokio::test]
async fn reconciliation_closes_every_login_crash_boundary() {
    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let vault = Arc::new(MemoryVault::default());
    let provision = VaultProvision::Available(vault.clone() as Arc<dyn Vault>);
    let mut accounts = memory_accounts();

    // Boundary A — claimed, nothing else (crash before vault write): stays
    // pending, waiting for the same command with a fresh stage.
    let identity_a = identity_for("profile-r", "command-a");
    let json_a = identity_a
        .canonical_json()
        .unwrap_or_else(|error| panic!("{error:?}"));
    let digest_a = blake3::hash(json_a.as_bytes()).to_hex().to_string();
    let _ = store
        .login_claim_receipt("command-a".into(), digest_a, json_a)
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));

    // Boundary B — vault written, descriptor missing (crash between
    // Keychain and descriptor save): resume the descriptor commit and
    // finalize.
    let identity_b = identity_for("profile-r", "command-b");
    let json_b = identity_b
        .canonical_json()
        .unwrap_or_else(|error| panic!("{error:?}"));
    let digest_b = blake3::hash(json_b.as_bytes()).to_hex().to_string();
    let _ = store
        .login_claim_receipt("command-b".into(), digest_b, json_b)
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
    vault
        .put(
            &CredentialAlias::new(identity_b.physical_alias.clone()),
            b"sk-vault-only",
        )
        .unwrap_or_else(|error| panic!("{error:?}"));

    // Boundary C — vault + descriptor, receipt still pending (crash before
    // finalization): finalize the committed response.
    let identity_c = identity_for("profile-r", "command-c");
    let json_c = identity_c
        .canonical_json()
        .unwrap_or_else(|error| panic!("{error:?}"));
    let digest_c = blake3::hash(json_c.as_bytes()).to_hex().to_string();
    let _ = store
        .login_claim_receipt("command-c".into(), digest_c, json_c)
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
    vault
        .put(
            &CredentialAlias::new(identity_c.physical_alias.clone()),
            b"sk-both",
        )
        .unwrap_or_else(|error| panic!("{error:?}"));
    accounts
        .add(descriptor_for(
            &identity_c,
            &CredentialAlias::new(identity_c.physical_alias.clone()),
            None,
        ))
        .unwrap_or_else(|error| panic!("{error:?}"));

    // Boundary D — committed receipt but the descriptor file lost the row
    // (descriptor-store rollback after the fact): self-heal from the
    // durable response without stealing active.
    let identity_d = identity_for("profile-r", "command-d");
    let json_d = identity_d
        .canonical_json()
        .unwrap_or_else(|error| panic!("{error:?}"));
    let digest_d = blake3::hash(json_d.as_bytes()).to_hex().to_string();
    let _ = store
        .login_claim_receipt("command-d".into(), digest_d, json_d)
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
    let descriptor_d = CredentialDescriptor {
        alias: CredentialAlias::new(identity_d.physical_alias.clone()),
        provider: "anthropic".into(),
        base_url: None,
        auth_method: AuthMethod::ApiKey,
        identity: "healed".into(),
        status: CredentialStatus::Ok,
        active: true,
    };
    store
        .finalize_login_receipt(
            "command-d".into(),
            LoginReceiptResponse {
                descriptor: descriptor_d.clone(),
            },
        )
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));

    reconcile_login_receipts(&store, &mut accounts, &provision)
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));

    // A: still pending (fresh-stage retry path).
    let rows = store
        .login_receipts()
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
    let state_of = |command: &str| {
        rows.iter()
            .find(|row| row.command_id == command)
            .map(|row| row.state.clone())
            .unwrap_or_else(|| panic!("receipt {command} missing"))
    };
    assert_eq!(state_of("command-a"), "pending");
    assert_eq!(state_of("command-b"), "committed");
    assert_eq!(state_of("command-c"), "committed");
    assert_eq!(state_of("command-d"), "committed");

    // B and C produced descriptors; D self-healed WITHOUT stealing the
    // provider's active slot (C's earlier active survives).
    assert!(
        accounts
            .get(&CredentialAlias::new(identity_b.physical_alias.clone()))
            .is_some()
    );
    assert!(
        accounts
            .get(&CredentialAlias::new(identity_d.physical_alias.clone()))
            .is_some_and(|descriptor| !descriptor.active)
    );
    assert!(
        accounts
            .get(&CredentialAlias::new(identity_a.physical_alias.clone()))
            .is_none(),
        "boundary A must not fabricate a descriptor"
    );

    store
        .close()
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
}

#[tokio::test]
async fn oauth_account_add_receipt_is_distinct_idempotent_and_secret_free() {
    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let identity = oauth_identity_for("profile-o", "oauth-command");
    let request_json = identity.canonical_json().expect("canonical OAuth identity");
    let digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
    for secret in [
        "OAUTH_ACCESS_RECEIPT_SENTINEL_1fa2",
        "OAUTH_REFRESH_RECEIPT_SENTINEL_9c31",
        "oauth-ready-ref-must-not-persist",
    ] {
        assert!(!request_json.contains(secret));
    }
    assert_eq!(
        store
            .account_add_claim_receipt(
                "oauth-command".into(),
                digest.clone(),
                request_json.clone(),
            )
            .await
            .expect("fresh"),
        AccountAddClaim::Fresh
    );
    assert_eq!(
        store
            .account_add_claim_receipt(
                "oauth-command".into(),
                digest.clone(),
                request_json.clone(),
            )
            .await
            .expect("resume"),
        AccountAddClaim::ResumePending
    );
    let descriptor = oauth_descriptor(
        &identity,
        &CredentialAlias::new(identity.physical_alias.clone()),
        &oauth_bundle(),
    );
    store
        .finalize_account_add_receipt(
            "oauth-command".into(),
            AccountAddReceiptResponse {
                descriptor: descriptor.clone(),
            },
        )
        .await
        .expect("finalize");
    assert!(matches!(
        store
            .account_add_claim_receipt("oauth-command".into(), digest, request_json)
            .await
            .expect("replay"),
        AccountAddClaim::Committed(response) if response.descriptor == descriptor
    ));

    // The method is deliberately not aliased to account.login_api.
    let login_identity = identity_for("profile-o", "oauth-command");
    let login_json = login_identity.canonical_json().expect("login identity");
    let login_digest = blake3::hash(login_json.as_bytes()).to_hex().to_string();
    let collision = store
        .login_claim_receipt("oauth-command".into(), login_digest, login_json)
        .await
        .expect_err("different durable methods cannot share a command id");
    assert!(collision.message.contains("different method or semantic"));

    store.close().await.expect("close");
}

#[tokio::test]
async fn oauth_account_add_never_exposes_initial_token_before_vault_persistence() {
    use std::sync::Condvar;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct BlockingPutVault {
        inner: MemoryVault,
        entered: AtomicBool,
        release: (StdMutex<bool>, Condvar),
    }

    impl Vault for BlockingPutVault {
        fn put(
            &self,
            alias: &CredentialAlias,
            secret: &[u8],
        ) -> haider_accounts::AccountsResult<()> {
            self.entered.store(true, Ordering::SeqCst);
            let (lock, wake) = &self.release;
            let mut released = lock.lock().expect("release lock");
            while !*released {
                released = wake.wait(released).expect("release wait");
            }
            self.inner.put(alias, secret)
        }

        fn resolve(
            &self,
            alias: &CredentialAlias,
        ) -> haider_accounts::AccountsResult<haider_accounts::SecretHandle> {
            self.inner.resolve(alias)
        }

        fn delete(&self, alias: &CredentialAlias) -> haider_accounts::AccountsResult<()> {
            self.inner.delete(alias)
        }

        fn list(&self) -> haider_accounts::AccountsResult<Vec<CredentialAlias>> {
            self.inner.list()
        }
    }

    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(Vec::new()));
    let vault = Arc::new(BlockingPutVault {
        inner: MemoryVault::new(),
        entered: AtomicBool::new(false),
        release: (StdMutex::new(false), Condvar::new()),
    });
    let mut actor = start_account_actor(AccountActorConfig {
        store: store.clone(),
        accounts: memory_accounts(),
        vault: vault.clone() as Arc<dyn Vault>,
        validator: Arc::new(ProviderCredentialValidator),
        snapshot: Arc::clone(&snapshot),
        management: None,
        profile_id: "profile-initial-persist".into(),
        default_model: "unused".into(),
        providers: test_provider_registry(),
        provider_endpoint_validator: Arc::new(ProductionProviderEndpointValidator),
        reserved_aliases: HashSet::new(),
        refresh_fences: RefreshFenceRegistry::default(),
    });
    let (sink, mut frames) = channel_sink();
    actor
        .commands()
        .send(AccountCommand::AddOAuth(Box::new(OAuthAddJob {
            command_id: "oauth-initial-persist".into(),
            provider: "fake-oauth".into(),
            display_alias: "work-oauth".into(),
            claim: Some(OAuthReadyClaim::for_account_test(
                "fake-oauth",
                "work-oauth",
                oauth_bundle(),
            )),
            route: LoginRoute {
                request_id: RequestId::new("oauth-initial-persist-request"),
                sink,
            },
        })))
        .await
        .expect("actor command");
    tokio::time::timeout(Duration::from_secs(1), async {
        while !vault.entered.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("vault put entered");
    let alias = CredentialAlias::new("work-oauth");
    assert!(frames.try_recv().is_err(), "no response before vault put");
    assert!(snapshot.lock().expect("snapshot").is_empty());
    assert!(
        vault.resolve(&alias).is_err(),
        "token is not yet resolvable"
    );
    {
        let (release, wake) = &vault.release;
        *release.lock().expect("release") = true;
        wake.notify_all();
    }
    let frame = tokio::time::timeout(Duration::from_secs(1), frames.recv())
        .await
        .expect("account response")
        .expect("account frame");
    assert!(matches!(
        frame,
        WireFrame::Response {
            body: ResponseBody::AccountAdd { .. },
            ..
        }
    ));
    assert!(vault.resolve(&alias).is_ok());
    assert_eq!(snapshot.lock().expect("snapshot").len(), 1);
    actor.shutdown().await;
    store.close().await.expect("close");
}

#[tokio::test]
async fn oauth_account_add_actor_crash_after_vault_put_reconciles_production_receipt() {
    use std::sync::Condvar;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct PutThenBlockVault {
        inner: MemoryVault,
        persisted: AtomicBool,
        release: (StdMutex<bool>, Condvar),
    }

    impl Vault for PutThenBlockVault {
        fn put(
            &self,
            alias: &CredentialAlias,
            secret: &[u8],
        ) -> haider_accounts::AccountsResult<()> {
            self.inner.put(alias, secret)?;
            self.persisted.store(true, Ordering::SeqCst);
            let (lock, wake) = &self.release;
            let mut released = lock.lock().expect("release lock");
            while !*released {
                released = wake.wait(released).expect("release wait");
            }
            Ok(())
        }

        fn resolve(
            &self,
            alias: &CredentialAlias,
        ) -> haider_accounts::AccountsResult<haider_accounts::SecretHandle> {
            self.inner.resolve(alias)
        }

        fn delete(&self, alias: &CredentialAlias) -> haider_accounts::AccountsResult<()> {
            self.inner.delete(alias)
        }

        fn list(&self) -> haider_accounts::AccountsResult<Vec<CredentialAlias>> {
            self.inner.list()
        }
    }

    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(Vec::new()));
    let vault = Arc::new(PutThenBlockVault {
        inner: MemoryVault::new(),
        persisted: AtomicBool::new(false),
        release: (StdMutex::new(false), Condvar::new()),
    });
    let actor = start_account_actor(AccountActorConfig {
        store: store.clone(),
        accounts: memory_accounts(),
        vault: vault.clone() as Arc<dyn Vault>,
        validator: Arc::new(ProviderCredentialValidator),
        snapshot: Arc::clone(&snapshot),
        management: None,
        profile_id: "profile-oauth-crash".into(),
        default_model: "unused".into(),
        providers: test_provider_registry(),
        provider_endpoint_validator: Arc::new(ProductionProviderEndpointValidator),
        reserved_aliases: HashSet::new(),
        refresh_fences: RefreshFenceRegistry::default(),
    });
    let (sink, mut frames) = channel_sink();
    actor
        .commands()
        .send(AccountCommand::AddOAuth(Box::new(OAuthAddJob {
            command_id: "oauth-crash-command".into(),
            provider: "fake-oauth".into(),
            display_alias: "work-oauth".into(),
            claim: Some(OAuthReadyClaim::for_account_test(
                "fake-oauth",
                "work-oauth",
                oauth_bundle(),
            )),
            route: LoginRoute {
                request_id: RequestId::new("oauth-crash-request"),
                sink,
            },
        })))
        .await
        .expect("actor command");
    tokio::time::timeout(Duration::from_secs(1), async {
        while !vault.persisted.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("vault persisted before crash");

    // Crash at the production seam: SQLite receipt is pending and the vault
    // write is durable, but the actor has not observed the worker return, so
    // neither descriptor nor response can have been published.
    actor.crash();
    assert!(snapshot.lock().expect("snapshot").is_empty());
    assert!(frames.try_recv().is_err());
    {
        let (release, wake) = &vault.release;
        *release.lock().expect("release") = true;
        wake.notify_all();
    }
    let alias = CredentialAlias::new("work-oauth");
    tokio::time::timeout(Duration::from_secs(1), async {
        while vault.resolve(&alias).is_err() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("durable vault value");

    let mut restarted_accounts = memory_accounts();
    reconcile_oauth_add_receipts(
        &store,
        &mut restarted_accounts,
        &VaultProvision::Available(vault.clone() as Arc<dyn Vault>),
    )
    .await
    .expect("restart reconciliation");
    assert!(
        restarted_accounts.get(&alias).is_some(),
        "restart reconstructs the descriptor from the durable bundle"
    );
    let receipts = store.account_add_receipts().await.expect("receipt rows");
    assert!(receipts.iter().any(|receipt| {
        receipt.command_id == "oauth-crash-command" && receipt.state == "committed"
    }));
    store.close().await.expect("close");
}

#[tokio::test]
async fn oauth_reconciliation_closes_crash_before_and_after_vault_put() {
    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let vault = Arc::new(MemoryVault::new());
    let provision = VaultProvision::Available(vault.clone() as Arc<dyn Vault>);
    let mut accounts = memory_accounts();

    // A: receipt claimed, crash before vault put. Browser/ready objects are
    // gone after restart, so this stays pending for a fresh flow.
    let identity_a = oauth_identity_for("profile-r", "oauth-a");
    let json_a = identity_a.canonical_json().expect("identity A");
    let digest_a = blake3::hash(json_a.as_bytes()).to_hex().to_string();
    assert_eq!(
        store
            .account_add_claim_receipt("oauth-a".into(), digest_a, json_a)
            .await
            .expect("claim A"),
        AccountAddClaim::Fresh
    );

    // B: vault bundle durable, crash before descriptor save. Reconciliation
    // verifies the versioned bundle, creates the OAuth descriptor, finalizes.
    let identity_b = oauth_identity_for("profile-r", "oauth-b");
    let json_b = identity_b.canonical_json().expect("identity B");
    let digest_b = blake3::hash(json_b.as_bytes()).to_hex().to_string();
    let _ = store
        .account_add_claim_receipt("oauth-b".into(), digest_b, json_b)
        .await
        .expect("claim B");
    vault
        .put(
            &CredentialAlias::new(identity_b.physical_alias.clone()),
            &oauth_bundle().encode().expect("encode B"),
        )
        .expect("vault B");

    // C: vault + descriptor committed, crash before receipt finalization.
    let identity_c = oauth_identity_for("profile-r", "oauth-c");
    let json_c = identity_c.canonical_json().expect("identity C");
    let digest_c = blake3::hash(json_c.as_bytes()).to_hex().to_string();
    let _ = store
        .account_add_claim_receipt("oauth-c".into(), digest_c, json_c)
        .await
        .expect("claim C");
    let bundle_c = oauth_bundle();
    vault
        .put(
            &CredentialAlias::new(identity_c.physical_alias.clone()),
            &bundle_c.encode().expect("encode C"),
        )
        .expect("vault C");
    accounts
        .add(oauth_descriptor(
            &identity_c,
            &CredentialAlias::new(identity_c.physical_alias.clone()),
            &bundle_c,
        ))
        .expect("descriptor C");

    // D: committed receipt survives a lost descriptor and self-heals without
    // stealing the active slot from the later credential.
    let identity_d = oauth_identity_for("profile-r", "oauth-d");
    let json_d = identity_d.canonical_json().expect("identity D");
    let digest_d = blake3::hash(json_d.as_bytes()).to_hex().to_string();
    let _ = store
        .account_add_claim_receipt("oauth-d".into(), digest_d, json_d)
        .await
        .expect("claim D");
    let mut descriptor_d = oauth_descriptor(
        &identity_d,
        &CredentialAlias::new(identity_d.physical_alias.clone()),
        &oauth_bundle(),
    );
    descriptor_d.active = true;
    store
        .finalize_account_add_receipt(
            "oauth-d".into(),
            AccountAddReceiptResponse {
                descriptor: descriptor_d.clone(),
            },
        )
        .await
        .expect("finalize D");

    reconcile_oauth_add_receipts(&store, &mut accounts, &provision)
        .await
        .expect("OAuth reconciliation");
    let rows = store.account_add_receipts().await.expect("receipt rows");
    let state = |command: &str| {
        rows.iter()
            .find(|row| row.command_id == command)
            .map(|row| row.state.as_str())
            .unwrap_or("missing")
    };
    assert_eq!(state("oauth-a"), "pending");
    assert_eq!(state("oauth-b"), "committed");
    assert_eq!(state("oauth-c"), "committed");
    assert_eq!(state("oauth-d"), "committed");
    assert!(
        accounts
            .get(&CredentialAlias::new(identity_a.physical_alias))
            .is_none()
    );
    assert!(
        accounts
            .get(&CredentialAlias::new(identity_b.physical_alias))
            .is_some()
    );
    assert!(
        accounts
            .get(&CredentialAlias::new(identity_d.physical_alias))
            .is_some_and(|descriptor| !descriptor.active)
    );

    store.close().await.expect("close");
}

#[tokio::test]
async fn refresh_cas_ignores_benign_status_and_selection_changes() {
    let revision_dir = test_store_dir();
    let revision_store = open_store(revision_dir.path()).await;
    let mut accounts = memory_accounts();
    let identity = oauth_identity_for("profile-cas", "oauth-cas");
    let alias = CredentialAlias::new(identity.physical_alias.clone());
    let original = oauth_bundle();
    let captured = oauth_descriptor(&identity, &alias, &original);
    accounts
        .add(captured.clone())
        .expect("add OAuth descriptor");
    let other_alias = CredentialAlias::new("other-oauth-account");
    accounts
        .add(CredentialDescriptor {
            alias: other_alias.clone(),
            provider: captured.provider.clone(),
            base_url: None,
            auth_method: AuthMethod::OAuth,
            identity: "other@example.invalid".into(),
            status: CredentialStatus::Ok,
            active: false,
        })
        .expect("add other descriptor");
    accounts
        .set_status(&alias, CredentialStatus::Limited { until_ms: 123_456 })
        .expect("limit captured descriptor");
    accounts
        .select(&other_alias)
        .expect("change active selection");
    let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(accounts.list().to_vec()));
    let vault = Arc::new(MemoryVault::new());
    vault
        .put(&alias, &original.encode().expect("encode original"))
        .expect("seed vault");
    let rotated = haider_accounts::OAuthTokenBundleV1::new(
        original.provider_id.clone(),
        original.issuer.clone(),
        original.audience.clone(),
        original.resource.clone(),
        "Bearer".into(),
        Zeroizing::new(b"ROTATED_CAS_ACCESS_58af".to_vec()),
        Some(Zeroizing::new(b"ROTATED_CAS_REFRESH_914b".to_vec())),
        u64::MAX - 2,
        Some(u64::MAX),
        original.granted_scopes.clone(),
        original.identity.clone(),
        2,
    )
    .expect("rotated bundle");
    apply_oauth_refresh(
        &mut accounts,
        vault.clone() as Arc<dyn Vault>,
        &snapshot,
        None,
        &revision_store,
        &captured,
        &OAuthRefreshFence {
            fence_epoch: 0,
            generation: 1,
            issuer: original.issuer.clone(),
            audience: original.audience.clone(),
            resource: original.resource.clone(),
            subject_hash: original.identity.subject_hash.clone(),
        },
        rotated.encode().expect("encode rotated"),
        &RefreshFenceRegistry::default(),
        &watch::channel(false).1,
    )
    .await
    .expect("benign fields do not fence rotation");
    let current = accounts.get(&alias).expect("descriptor remains");
    assert!(!current.active);
    assert!(matches!(
        current.status,
        CredentialStatus::Limited { until_ms: 123_456 }
    ));
    let stored = vault.resolve(&alias).expect("rotated vault");
    assert_eq!(
        haider_accounts::OAuthTokenBundleV1::decode(stored.expose_secret())
            .expect("decode rotated")
            .generation,
        2
    );
}

#[tokio::test]
async fn production_expiry_sink_rejects_a_late_failure_for_a_newer_generation() {
    let revision_dir = test_store_dir();
    let revision_store = open_store(revision_dir.path()).await;
    let mut accounts = memory_accounts();
    let identity = oauth_identity_for("profile-expiry-fence", "oauth-fence");
    let alias = CredentialAlias::new(identity.physical_alias.clone());
    let original = oauth_bundle();
    let captured = oauth_descriptor(&identity, &alias, &original);
    accounts
        .add(captured.clone())
        .expect("add captured descriptor");
    let vault = Arc::new(MemoryVault::new());
    let replacement = haider_accounts::OAuthTokenBundleV1::new(
        original.provider_id.clone(),
        original.issuer.clone(),
        original.audience.clone(),
        original.resource.clone(),
        "Bearer".into(),
        Zeroizing::new(b"NEWER_EXPIRY_FENCE_ACCESS_41c8".to_vec()),
        Some(Zeroizing::new(b"NEWER_EXPIRY_FENCE_REFRESH_911d".to_vec())),
        u64::MAX - 2,
        Some(u64::MAX),
        original.granted_scopes.clone(),
        original.identity.clone(),
        9,
    )
    .expect("replacement");
    vault
        .put(&alias, &replacement.encode().expect("encode replacement"))
        .expect("seed replacement");
    let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(accounts.list().to_vec()));
    let expired = expire_oauth_refresh(
        &mut accounts,
        vault.clone() as Arc<dyn Vault>,
        &snapshot,
        None,
        &revision_store,
        &captured,
        &OAuthRefreshFence {
            fence_epoch: 0,
            generation: 1,
            issuer: original.issuer.clone(),
            audience: original.audience.clone(),
            resource: original.resource.clone(),
            subject_hash: original.identity.subject_hash.clone(),
        },
        &RefreshFenceRegistry::default(),
    )
    .await
    .expect("expiry sink");
    assert!(!expired, "late failure must be a no-op");
    assert!(matches!(
        accounts.get(&alias).expect("replacement descriptor").status,
        CredentialStatus::Ok
    ));
    assert_eq!(
        haider_accounts::OAuthTokenBundleV1::decode(
            vault
                .resolve(&alias)
                .expect("replacement retained")
                .expose_secret()
        )
        .expect("decode replacement")
        .generation,
        9
    );
}

#[tokio::test]
async fn production_refresh_begin_durably_fences_restart_before_the_endpoint() {
    struct SharedDescriptorStore(Arc<StdMutex<Vec<CredentialDescriptor>>>);

    impl StoreLike for SharedDescriptorStore {
        fn load(&self) -> Result<Vec<CredentialDescriptor>, HaiderError> {
            Ok(self
                .0
                .lock()
                .map(|descriptors| descriptors.clone())
                .unwrap_or_default())
        }

        fn save(&self, descriptors: &[CredentialDescriptor]) -> Result<(), HaiderError> {
            if let Ok(mut durable) = self.0.lock() {
                *durable = descriptors.to_vec();
            }
            Ok(())
        }
    }

    let durable = Arc::new(StdMutex::new(Vec::new()));
    let revision_dir = test_store_dir();
    let revision_store = open_store(revision_dir.path()).await;
    let store: Box<dyn StoreLike> = Box::new(SharedDescriptorStore(Arc::clone(&durable)));
    let mut accounts = AccountStore::new(store).expect("accounts");
    let identity = oauth_identity_for("profile-refresh-begin", "oauth-begin");
    let alias = CredentialAlias::new(identity.physical_alias.clone());
    let original = oauth_bundle();
    let descriptor = oauth_descriptor(&identity, &alias, &original);
    accounts.add(descriptor.clone()).expect("seed descriptor");
    let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(accounts.list().to_vec()));
    let vault = Arc::new(MemoryVault::new());
    vault
        .put(&alias, &original.encode().expect("encode original"))
        .expect("seed vault");
    let begun = begin_oauth_refresh(
        &mut accounts,
        vault.clone() as Arc<dyn Vault>,
        &snapshot,
        None,
        &revision_store,
        &descriptor,
        &OAuthRefreshFence {
            fence_epoch: 0,
            generation: original.generation,
            issuer: original.issuer.clone(),
            audience: original.audience.clone(),
            resource: original.resource.clone(),
            subject_hash: original.identity.subject_hash.clone(),
        },
        &RefreshFenceRegistry::default(),
    )
    .await
    .expect("begin");
    assert!(begun);
    assert!(matches!(
        snapshot.lock().expect("snapshot")[0].status,
        CredentialStatus::Expired
    ));
    assert!(
        vault.resolve(&alias).is_ok(),
        "ordinary begin retains the original bundle for audit"
    );

    let restarted: Box<dyn StoreLike> = Box::new(SharedDescriptorStore(durable));
    let restarted = AccountStore::new(restarted).expect("restart descriptors");
    assert!(matches!(
        restarted.get(&alias).expect("descriptor").status,
        CredentialStatus::Expired
    ));
}

#[tokio::test]
async fn production_descriptor_failure_tombstones_the_dead_rotated_refresh_token() {
    use std::sync::atomic::{AtomicBool, Ordering};

    struct FailingDescriptorStore {
        durable: Arc<StdMutex<Vec<CredentialDescriptor>>>,
        fail: Arc<AtomicBool>,
    }

    impl StoreLike for FailingDescriptorStore {
        fn load(&self) -> Result<Vec<CredentialDescriptor>, HaiderError> {
            Ok(self
                .durable
                .lock()
                .map(|descriptors| descriptors.clone())
                .unwrap_or_default())
        }

        fn save(&self, descriptors: &[CredentialDescriptor]) -> Result<(), HaiderError> {
            if self.fail.load(Ordering::SeqCst) {
                return Err(HaiderError::new(
                    ErrorCode::ProviderError,
                    "injected production descriptor persistence failure",
                    false,
                ));
            }
            if let Ok(mut durable) = self.durable.lock() {
                *durable = descriptors.to_vec();
            }
            Ok(())
        }
    }

    struct FailingRotationVault {
        inner: MemoryVault,
        fail_put: AtomicBool,
    }

    impl Vault for FailingRotationVault {
        fn put(
            &self,
            alias: &CredentialAlias,
            secret: &[u8],
        ) -> haider_accounts::AccountsResult<()> {
            if self.fail_put.load(Ordering::SeqCst) {
                return Err(HaiderError::new(
                    ErrorCode::ProviderError,
                    "injected rotated bundle persistence failure",
                    false,
                ));
            }
            self.inner.put(alias, secret)
        }

        fn resolve(
            &self,
            alias: &CredentialAlias,
        ) -> haider_accounts::AccountsResult<haider_accounts::SecretHandle> {
            self.inner.resolve(alias)
        }

        fn delete(&self, alias: &CredentialAlias) -> haider_accounts::AccountsResult<()> {
            self.inner.delete(alias)
        }

        fn list(&self) -> haider_accounts::AccountsResult<Vec<CredentialAlias>> {
            self.inner.list()
        }
    }

    let durable = Arc::new(StdMutex::new(Vec::new()));
    let revision_dir = test_store_dir();
    let revision_store = open_store(revision_dir.path()).await;
    let fail_descriptor = Arc::new(AtomicBool::new(false));
    let store: Box<dyn StoreLike> = Box::new(FailingDescriptorStore {
        durable: Arc::clone(&durable),
        fail: Arc::clone(&fail_descriptor),
    });
    let mut accounts = AccountStore::new(store).expect("accounts");
    let identity = oauth_identity_for("profile-durable-expiry", "oauth-refresh");
    let alias = CredentialAlias::new(identity.physical_alias.clone());
    let original = oauth_bundle();
    let descriptor = oauth_descriptor(&identity, &alias, &original);
    accounts.add(descriptor.clone()).expect("seed descriptor");
    let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(accounts.list().to_vec()));
    let vault = Arc::new(FailingRotationVault {
        inner: MemoryVault::new(),
        fail_put: AtomicBool::new(false),
    });
    vault
        .put(&alias, &original.encode().expect("encode original"))
        .expect("seed vault");
    vault.fail_put.store(true, Ordering::SeqCst);
    fail_descriptor.store(true, Ordering::SeqCst);

    let rotated = haider_accounts::OAuthTokenBundleV1::new(
        original.provider_id.clone(),
        original.issuer.clone(),
        original.audience.clone(),
        original.resource.clone(),
        "Bearer".into(),
        Zeroizing::new(b"ROTATED_DURABILITY_ACCESS_d431".to_vec()),
        Some(Zeroizing::new(b"ROTATED_DURABILITY_REFRESH_9e2a".to_vec())),
        u64::MAX - 2,
        Some(u64::MAX),
        original.granted_scopes.clone(),
        original.identity.clone(),
        2,
    )
    .expect("rotated");
    assert!(matches!(
        apply_oauth_refresh(
            &mut accounts,
            vault.clone() as Arc<dyn Vault>,
            &snapshot,
            None,
            &revision_store,
            &descriptor,
            &OAuthRefreshFence {
                fence_epoch: 0,
                generation: 1,
                issuer: original.issuer.clone(),
                audience: original.audience.clone(),
                resource: original.resource.clone(),
                subject_hash: original.identity.subject_hash.clone(),
            },
            rotated.encode().expect("encode rotated"),
            &RefreshFenceRegistry::default(),
            &watch::channel(false).1,
        )
        .await,
        Err(RefreshApplyError::Persist)
    ));
    assert!(
        vault.resolve(&alias).is_err(),
        "descriptor persistence failure must durably delete the dead token"
    );
    assert!(matches!(
        snapshot.lock().expect("snapshot")[0].status,
        CredentialStatus::Expired
    ));
    assert!(matches!(
        durable.lock().expect("durable")[0].status,
        CredentialStatus::Ok
    ));

    // Restart sees the production descriptor save failure, but the durable
    // vault tombstone prevents any resolver from obtaining/retrying the dead
    // generation-1 refresh token.
    let restarted: Box<dyn StoreLike> = Box::new(FailingDescriptorStore {
        durable,
        fail: fail_descriptor,
    });
    let restarted = AccountStore::new(restarted).expect("restart descriptors");
    assert!(matches!(
        restarted.get(&alias).expect("descriptor").status,
        CredentialStatus::Ok
    ));
    assert!(vault.resolve(&alias).is_err());
}

/// Durable set-active/remove publication, deterministic successor selection,
/// committed replay, and the remove/re-add refresh epoch all run through the
/// production account actor.
///
/// MUTATION CHECK: remove `refresh_fences.invalidate(&alias)` from
/// `handle_remove_account`. Expected runtime failure: the late refresh below
/// returns `Ok(())` and replaces the re-added generation-one vault bundle
/// with `LATE_REMOVE_REFRESH_ACCESS_7cc1`.
#[tokio::test]
async fn durable_remove_fences_late_refresh_across_same_alias_readd() {
    let bundle = oauth_bundle();
    let primary_alias = CredentialAlias::new("oauth-primary");
    let removed_alias = CredentialAlias::new("oauth-removed");
    let descriptor = |alias: CredentialAlias, active: bool| CredentialDescriptor {
        alias,
        provider: "fake-oauth".into(),
        base_url: None,
        auth_method: AuthMethod::OAuth,
        identity: bundle.identity.display_identity.clone(),
        status: CredentialStatus::Ok,
        active,
    };
    let mut accounts = memory_accounts();
    accounts
        .add(descriptor(primary_alias.clone(), true))
        .expect("primary descriptor");
    accounts
        .add(descriptor(removed_alias.clone(), false))
        .expect("secondary descriptor");

    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let vault = Arc::new(MemoryVault::new());
    let encoded = bundle.encode().expect("encode original bundle");
    vault
        .put(&primary_alias, &encoded)
        .expect("seed primary bundle");
    vault
        .put(&removed_alias, &encoded)
        .expect("seed removable bundle");
    let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(accounts.list().to_vec()));
    let management = ManagementSnapshot::new(0, accounts.list().to_vec(), Vec::new());
    let refresh_fences = RefreshFenceRegistry::default();
    let mut actor = start_account_actor(AccountActorConfig {
        store: store.clone(),
        accounts,
        vault: vault.clone() as Arc<dyn Vault>,
        validator: Arc::new(ProviderCredentialValidator),
        snapshot: Arc::clone(&snapshot),
        management: Some(management.clone()),
        profile_id: "remove-refresh-fence".into(),
        default_model: "unused".into(),
        providers: test_provider_registry(),
        provider_endpoint_validator: Arc::new(ProductionProviderEndpointValidator),
        reserved_aliases: HashSet::new(),
        refresh_fences: refresh_fences.clone(),
    });
    let commands = actor.commands();
    let (sink, mut frames) = channel_sink();

    commands
        .send(AccountCommand::SetActive(Box::new(SetActiveJob {
            command_id: "set-active-before-remove".into(),
            alias: removed_alias.as_str().into(),
            route: LoginRoute {
                request_id: RequestId::new("set-active-before-remove-request"),
                sink: Arc::clone(&sink),
            },
        })))
        .await
        .expect("send set-active");
    match tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("set-active deadline")
        .expect("set-active response")
    {
        WireFrame::Response {
            body:
                ResponseBody::AccountSetActive {
                    descriptor,
                    prior_alias,
                    revision,
                },
            ..
        } => {
            assert_eq!(descriptor.alias, removed_alias);
            assert_eq!(prior_alias, Some(primary_alias.clone()));
            assert_eq!(revision, 1);
        }
        other => panic!("unexpected set-active response: {other:?}"),
    }

    commands
        .send(AccountCommand::Remove(Box::new(RemoveAccountJob {
            command_id: "remove-active-oauth".into(),
            alias: removed_alias.as_str().into(),
            expected_revision: Some(1),
            route: LoginRoute {
                request_id: RequestId::new("remove-active-oauth-request"),
                sink: Arc::clone(&sink),
            },
        })))
        .await
        .expect("send remove");
    match tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("remove deadline")
        .expect("remove response")
    {
        WireFrame::Response {
            body:
                ResponseBody::AccountRemove {
                    removed_alias: response_alias,
                    replacement_active_alias,
                    revision,
                },
            ..
        } => {
            assert_eq!(response_alias, removed_alias);
            assert_eq!(replacement_active_alias, Some(primary_alias.clone()));
            assert_eq!(revision, 2);
        }
        other => panic!("unexpected remove response: {other:?}"),
    }
    assert!(vault.resolve(&removed_alias).is_err());
    assert!(
        store
            .reserved_account_aliases()
            .await
            .expect("reservations")
            .is_empty()
    );
    let after_remove = management.read().expect("management snapshot");
    assert_eq!(after_remove.revision, 2);
    assert_eq!(after_remove.descriptors.len(), 1);
    assert_eq!(after_remove.descriptors[0].alias, primary_alias);
    assert!(after_remove.descriptors[0].active);
    drop(after_remove);

    commands
        .send(AccountCommand::AddOAuth(Box::new(OAuthAddJob {
            command_id: "readd-removed-oauth".into(),
            provider: "fake-oauth".into(),
            display_alias: removed_alias.as_str().into(),
            claim: Some(OAuthReadyClaim::for_account_test(
                "fake-oauth",
                removed_alias.as_str(),
                oauth_bundle(),
            )),
            route: LoginRoute {
                request_id: RequestId::new("readd-removed-oauth-request"),
                sink: Arc::clone(&sink),
            },
        })))
        .await
        .expect("send re-add");
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), frames.recv())
            .await
            .expect("re-add deadline")
            .expect("re-add response"),
        WireFrame::Response {
            body: ResponseBody::AccountAdd { .. },
            ..
        }
    ));

    let late = haider_accounts::OAuthTokenBundleV1::new(
        bundle.provider_id.clone(),
        bundle.issuer.clone(),
        bundle.audience.clone(),
        bundle.resource.clone(),
        "Bearer".into(),
        Zeroizing::new(b"LATE_REMOVE_REFRESH_ACCESS_7cc1".to_vec()),
        Some(Zeroizing::new(b"LATE_REMOVE_REFRESH_REFRESH_91ea".to_vec())),
        u64::MAX - 2,
        Some(u64::MAX),
        bundle.granted_scopes.clone(),
        bundle.identity.clone(),
        2,
    )
    .expect("late bundle");
    let (completed, result) = tokio::sync::oneshot::channel();
    commands
        .send(AccountCommand::ApplyOAuthRefresh {
            descriptor: descriptor(removed_alias.clone(), true),
            expected: OAuthRefreshFence {
                fence_epoch: 0,
                generation: bundle.generation,
                issuer: bundle.issuer.clone(),
                audience: bundle.audience.clone(),
                resource: bundle.resource.clone(),
                subject_hash: bundle.identity.subject_hash.clone(),
            },
            encoded_bundle: late.encode().expect("encode late bundle"),
            completed,
        })
        .await
        .expect("send late refresh");
    assert!(matches!(
        result.await.expect("late refresh completion"),
        Err(RefreshApplyError::Stale)
    ));
    let retained = haider_accounts::OAuthTokenBundleV1::decode(
        vault
            .resolve(&removed_alias)
            .expect("re-added bundle retained")
            .expose_secret(),
    )
    .expect("decode retained bundle");
    assert_eq!(retained.generation, 1);

    commands
        .send(AccountCommand::SetActive(Box::new(SetActiveJob {
            command_id: "set-active-before-remove".into(),
            alias: removed_alias.as_str().into(),
            route: LoginRoute {
                request_id: RequestId::new("set-active-replay-request"),
                sink,
            },
        })))
        .await
        .expect("send set-active replay");
    match tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("replay deadline")
        .expect("replay response")
    {
        WireFrame::Response {
            body: ResponseBody::AccountSetActive { revision, .. },
            ..
        } => assert_eq!(revision, 1),
        other => panic!("unexpected replay response: {other:?}"),
    }

    actor.shutdown().await;
}

/// An omitted API alias is command-derived before secret validation and is
/// retained in the durable semantic identity for replay.
///
/// MUTATION CHECK: hash `provider.as_bytes()` instead of
/// `command_id.as_bytes()` in `canonical_api_alias`. Expected runtime
/// failure: the response alias differs from the independently command-derived
/// alias asserted below.
#[tokio::test]
async fn omitted_api_alias_is_stable_command_derived_and_secret_free() {
    struct SuccessValidator;

    #[async_trait::async_trait]
    impl CredentialValidator for SuccessValidator {
        fn supports(&self, provider: &str) -> bool {
            provider == "anthropic"
        }

        async fn validate(
            &self,
            _provider: &str,
            _model: &str,
            _secret: &[u8],
        ) -> Result<ValidatedIdentity, ValidationError> {
            Ok(ValidatedIdentity {
                identity: "validated-person@example.invalid".into(),
            })
        }
    }

    let command_id = "omitted-alias-command";
    let mut expected_hasher = blake3::Hasher::new();
    expected_hasher.update(b"haider-account-api-alias-v1\n");
    expected_hasher.update(command_id.as_bytes());
    let expected_digest = expected_hasher.finalize().to_hex();
    let expected_alias = format!("anthropic-api-{}", &expected_digest.as_str()[..12]);
    assert_eq!(canonical_api_alias("anthropic", command_id), expected_alias);
    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let vault = Arc::new(MemoryVault::new());
    let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(Vec::new()));
    let mut actor = start_account_actor(AccountActorConfig {
        store: store.clone(),
        accounts: memory_accounts(),
        vault: vault.clone() as Arc<dyn Vault>,
        validator: Arc::new(SuccessValidator),
        snapshot,
        management: None,
        profile_id: "omitted-alias".into(),
        default_model: "claude-test".into(),
        providers: test_provider_registry(),
        provider_endpoint_validator: Arc::new(ProductionProviderEndpointValidator),
        reserved_aliases: HashSet::new(),
        refresh_fences: RefreshFenceRegistry::default(),
    });
    let commands = actor.commands();
    let (sink, mut frames) = channel_sink();
    commands
        .send(AccountCommand::Login(Box::new(LoginJob {
            command_id: command_id.into(),
            provider: "anthropic".into(),
            display_alias: None,
            validation_model: Some("claude-test".into()),
            secret: Some(Zeroizing::new(
                b"OMITTED_ALIAS_SECRET_SENTINEL_86b2".to_vec(),
            )),
            route: LoginRoute {
                request_id: RequestId::new("omitted-alias-request"),
                sink: Arc::clone(&sink),
            },
        })))
        .await
        .expect("send login");
    match tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("login deadline")
        .expect("login response")
    {
        WireFrame::Response {
            body: ResponseBody::AccountLoginApi { descriptor },
            ..
        } => {
            assert_eq!(descriptor.alias.as_str(), expected_alias);
            assert_eq!(descriptor.identity, "validated-person@example.invalid");
        }
        other => panic!("unexpected login response: {other:?}"),
    }
    assert!(
        vault
            .resolve(&CredentialAlias::new(expected_alias.clone()))
            .is_ok()
    );
    let receipts = store.login_receipts().await.expect("login receipts");
    let identity: LoginIdentity =
        serde_json::from_str(&receipts[0].request_json).expect("login identity");
    assert_eq!(
        identity.display_alias.as_deref(),
        Some(expected_alias.as_str())
    );
    assert_eq!(identity.physical_alias, expected_alias);
    assert!(!receipts[0].request_json.contains("SENTINEL"));
    assert!(!receipts[0].request_json.contains("validated-person"));

    commands
        .send(AccountCommand::Login(Box::new(LoginJob {
            command_id: command_id.into(),
            provider: "anthropic".into(),
            display_alias: None,
            validation_model: Some("claude-test".into()),
            secret: None,
            route: LoginRoute {
                request_id: RequestId::new("omitted-alias-replay"),
                sink,
            },
        })))
        .await
        .expect("send replay");
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), frames.recv())
            .await
            .expect("replay deadline")
            .expect("replay response"),
        WireFrame::Response {
            body: ResponseBody::AccountLoginApi { .. },
            ..
        }
    ));
    actor.shutdown().await;
}

/// Provider configuration and default-model mutation share the account actor,
/// coherent snapshot, receipt replay, and revision CAS.
///
/// MUTATION CHECK: move `validate_default_model` above
/// `management_receipt_preflight` in `handle_set_default_model`. Expected
/// runtime failure: replaying `set-custom-default-b` after model B is removed
/// returns `invalid_argument` instead of its committed revision-two receipt.
#[tokio::test]
async fn provider_mutations_replay_before_validation_and_publish_one_snapshot() {
    struct CanonicalEndpointValidator {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ProviderEndpointValidator for CanonicalEndpointValidator {
        async fn validate(&self, origin: &str) -> Result<String, HaiderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(origin.trim_end_matches('/').to_owned())
        }
    }

    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let accounts = memory_accounts();
    let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(Vec::new()));
    let management = ManagementSnapshot::new(0, Vec::new(), Vec::new());
    let endpoint_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut actor = start_account_actor(AccountActorConfig {
        store: store.clone(),
        accounts,
        vault: Arc::new(MemoryVault::new()),
        validator: Arc::new(ProviderCredentialValidator),
        snapshot,
        management: Some(management.clone()),
        profile_id: "provider-mutations".into(),
        default_model: "unused".into(),
        providers: test_provider_registry(),
        provider_endpoint_validator: Arc::new(CanonicalEndpointValidator {
            calls: Arc::clone(&endpoint_calls),
        }),
        reserved_aliases: HashSet::new(),
        refresh_fences: RefreshFenceRegistry::default(),
    });
    let commands = actor.commands();
    let (sink, mut frames) = channel_sink();

    let configure = |command_id: &str,
                     request_id: &str,
                     input: ProviderConfigureInput,
                     expected_revision: u64| {
        AccountCommand::ConfigureProvider(Box::new(ProviderConfigureJob {
            command_id: command_id.into(),
            input,
            expected_revision,
            route: LoginRoute {
                request_id: RequestId::new(request_id),
                sink: Arc::clone(&sink),
            },
        }))
    };
    commands
        .send(configure(
            "configure-custom",
            "configure-custom-request",
            ProviderConfigureInput {
                provider: "custom".into(),
                api_family: Some(ProviderApiFamilyWire::OpenAiChatCompletions),
                origin: Some("https://models.example.invalid/".into()),
                auth_requirement: Some(ProviderAuthRequirementWire::ApiKey),
                enabled: true,
                models: vec!["model-a".into(), "model-b".into()],
                default_model: Some("model-a".into()),
            },
            0,
        ))
        .await
        .expect("send configure");
    match tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("configure deadline")
        .expect("configure response")
    {
        WireFrame::Response {
            body: ResponseBody::ProviderConfigure { provider, revision },
            ..
        } => {
            assert_eq!(provider.provider, "custom");
            assert_eq!(
                provider.endpoint.as_deref(),
                Some("https://models.example.invalid")
            );
            assert_eq!(provider.default_model.as_deref(), Some("model-a"));
            assert_eq!(revision, 1);
        }
        other => panic!("unexpected configure response: {other:?}"),
    }
    assert_eq!(endpoint_calls.load(Ordering::SeqCst), 1);

    let set_default = |command_id: &str, request_id: &str, model: &str, expected_revision| {
        AccountCommand::SetDefaultModel(Box::new(SetDefaultModelJob {
            command_id: command_id.into(),
            provider: "custom".into(),
            model: model.into(),
            expected_revision,
            route: LoginRoute {
                request_id: RequestId::new(request_id),
                sink: Arc::clone(&sink),
            },
        }))
    };
    commands
        .send(set_default(
            "set-custom-default-b",
            "set-custom-default-b-request",
            "model-b",
            1,
        ))
        .await
        .expect("send default");
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), frames.recv())
            .await
            .expect("default deadline")
            .expect("default response"),
        WireFrame::Response {
            body: ResponseBody::AccountSetDefaultModel { revision: 2, .. },
            ..
        }
    ));

    commands
        .send(configure(
            "configure-custom-models",
            "configure-custom-models-request",
            ProviderConfigureInput {
                provider: "custom".into(),
                api_family: None,
                origin: None,
                auth_requirement: None,
                enabled: true,
                models: vec!["model-a".into()],
                default_model: Some("model-a".into()),
            },
            2,
        ))
        .await
        .expect("send model update");
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), frames.recv())
            .await
            .expect("model update deadline")
            .expect("model update response"),
        WireFrame::Response {
            body: ResponseBody::ProviderConfigure { revision: 3, .. },
            ..
        }
    ));
    assert_eq!(endpoint_calls.load(Ordering::SeqCst), 1);

    commands
        .send(set_default(
            "set-custom-default-b",
            "set-custom-default-b-replay",
            "model-b",
            1,
        ))
        .await
        .expect("send committed replay");
    match tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("replay deadline")
        .expect("replay response")
    {
        WireFrame::Response {
            body: ResponseBody::AccountSetDefaultModel { provider, revision },
            ..
        } => {
            assert_eq!(revision, 2);
            assert_eq!(provider.default_model.as_deref(), Some("model-b"));
        }
        other => panic!("unexpected default replay: {other:?}"),
    }

    commands
        .send(set_default(
            "stale-custom-default",
            "stale-custom-default-request",
            "model-b",
            1,
        ))
        .await
        .expect("send stale mutation");
    match tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("stale deadline")
        .expect("stale response")
    {
        WireFrame::Response {
            body:
                ResponseBody::Error {
                    code,
                    retryable,
                    data:
                        Some(ErrorData::RevisionConflict {
                            expected_revision,
                            current_revision,
                        }),
                    ..
                },
            ..
        } => {
            assert_eq!(code, ERROR_CODE_REVISION_CONFLICT);
            assert!(retryable);
            assert_eq!(expected_revision, 1);
            assert_eq!(current_revision, 3);
        }
        other => panic!("unexpected stale response: {other:?}"),
    }

    let view = management.read().expect("management snapshot");
    assert_eq!(view.revision, 3);
    let custom = view
        .providers
        .iter()
        .find(|provider| provider.provider == "custom")
        .expect("custom provider");
    assert_eq!(custom.models, vec!["model-a"]);
    assert_eq!(custom.default_model.as_deref(), Some("model-a"));
    drop(view);
    actor.shutdown().await;
}

/// Startup reconciliation keeps a failed vault deletion pending and reserved,
/// then completes it idempotently on the next reconciliation attempt.
///
/// MUTATION CHECK: remove the `continue` from the failed-delete branch in
/// `reconcile_remove_receipts`, allowing receipt finalization after a failed
/// vault delete. Expected runtime failure: the first-phase reservation and
/// pending-receipt assertions below observe an empty/committed state while
/// the orphan secret is still present.
#[tokio::test]
async fn pending_remove_reconciliation_retries_orphan_deletion_before_release() {
    struct DeleteGateVault {
        inner: MemoryVault,
        fail_delete: AtomicBool,
    }

    impl Vault for DeleteGateVault {
        fn put(
            &self,
            alias: &CredentialAlias,
            secret: &[u8],
        ) -> haider_accounts::AccountsResult<()> {
            self.inner.put(alias, secret)
        }

        fn resolve(
            &self,
            alias: &CredentialAlias,
        ) -> haider_accounts::AccountsResult<haider_accounts::SecretHandle> {
            self.inner.resolve(alias)
        }

        fn delete(&self, alias: &CredentialAlias) -> haider_accounts::AccountsResult<()> {
            if self.fail_delete.load(Ordering::SeqCst) {
                return Err(HaiderError::new(
                    ErrorCode::Internal,
                    "injected vault deletion failure",
                    true,
                ));
            }
            self.inner.delete(alias)
        }

        fn list(&self) -> haider_accounts::AccountsResult<Vec<CredentialAlias>> {
            self.inner.list()
        }
    }

    let alias = CredentialAlias::new("remove-orphan");
    let mut accounts = memory_accounts();
    accounts
        .add(CredentialDescriptor {
            alias: alias.clone(),
            provider: "anthropic".into(),
            base_url: None,
            auth_method: AuthMethod::ApiKey,
            identity: "orphan fixture".into(),
            status: CredentialStatus::Ok,
            active: true,
        })
        .expect("descriptor");
    let vault = Arc::new(DeleteGateVault {
        inner: MemoryVault::new(),
        fail_delete: AtomicBool::new(true),
    });
    vault
        .put(&alias, b"ORPHAN_REMOVE_SECRET_441a")
        .expect("seed secret");
    let provision = VaultProvision::Available(vault.clone() as Arc<dyn Vault>);
    let dir = test_store_dir();
    let store = open_store(dir.path()).await;
    let request_json = r#"{"alias":"remove-orphan","expected_revision":0}"#.to_owned();
    let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
    assert!(matches!(
        store
            .account_remove_claim_receipt::<RemoveReceipt>(
                "pending-remove-orphan".into(),
                request_digest,
                request_json,
                r#"{"provider":"anthropic","was_active":true}"#.into(),
                Some(0),
                alias.as_str().into(),
                "anthropic".into(),
                true,
            )
            .await
            .expect("claim remove"),
        ManagementClaim::Fresh
    ));

    reconcile_remove_receipts(&store, &mut accounts, &provision)
        .await
        .expect("first reconciliation");
    assert!(accounts.get(&alias).is_none());
    assert!(vault.resolve(&alias).is_ok(), "failed delete leaves orphan");
    assert_eq!(
        store
            .reserved_account_aliases()
            .await
            .expect("reserved aliases"),
        vec![alias.as_str().to_owned()]
    );
    assert_eq!(
        store
            .account_remove_receipts()
            .await
            .expect("pending receipt")
            .len(),
        1
    );
    assert_eq!(store.management_revision().await.expect("revision"), 0);

    vault.fail_delete.store(false, Ordering::SeqCst);
    reconcile_remove_receipts(&store, &mut accounts, &provision)
        .await
        .expect("second reconciliation");
    assert!(vault.resolve(&alias).is_err());
    assert!(
        store
            .reserved_account_aliases()
            .await
            .expect("released reservation")
            .is_empty()
    );
    assert!(
        store
            .account_remove_receipts()
            .await
            .expect("finalized receipt")
            .is_empty()
    );
    assert_eq!(store.management_revision().await.expect("revision"), 1);
}

/// Release evidence, never the merge gate (§6.2): the PACKAGED default
/// model ID must validate a real key through the production
/// `AnthropicValidator` (the same audited one-token Messages path `/login`
/// uses). Run manually: `HAIDER_ANTHROPIC_API_KEY=... cargo test -p
/// haider-daemon --lib live_smoke -- --ignored`.
#[tokio::test]
#[ignore = "live Anthropic API smoke; requires HAIDER_ANTHROPIC_API_KEY"]
async fn live_smoke_packaged_default_model_validates_a_real_key() {
    let key = std::env::var("HAIDER_ANTHROPIC_API_KEY")
        .unwrap_or_else(|_| panic!("HAIDER_ANTHROPIC_API_KEY must be set for the live smoke"));
    let identity = AnthropicValidator
        .validate(
            "anthropic",
            haider_client::PACKAGED_DEFAULT_MODEL,
            key.trim().as_bytes(),
        )
        .await
        .unwrap_or_else(|error| {
            panic!(
                "packaged default model {} failed live validation: {:?} {}",
                haider_client::PACKAGED_DEFAULT_MODEL,
                error.kind,
                error.message
            )
        });
    assert!(!identity.identity.is_empty());
}
