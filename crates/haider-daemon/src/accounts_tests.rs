//! Unit tests for the daemon account seam: physical-alias namespacing,
//! staged-secret laws, the login receipt shape, and the R10 startup
//! reconciliation over every crash boundary.
#![allow(clippy::expect_used)]

use super::*;
use haider_core::SqliteStoreHandle;

use crate::session_hub::FrameSendError;
use crate::worker::ProviderFactory as _;

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
        "profile-ttl",
        "claude-test",
        &mut pending,
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
        "profile-ttl",
        "claude-test",
        &mut pending,
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
    let actor = start_account_actor(AccountActorConfig {
        store: store.clone(),
        accounts: memory_accounts(),
        vault: Arc::new(MemoryVault::default()) as Arc<dyn Vault>,
        validator: Arc::clone(&validator) as Arc<dyn CredentialValidator>,
        snapshot,
        profile_id: "profile-ttl-actor".into(),
        default_model: "claude-test".into(),
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
