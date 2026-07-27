//! Unit tests for the daemon account seam: physical-alias namespacing,
//! staged-secret laws, the login receipt shape, and the R10 startup
//! reconciliation over every crash boundary.
#![allow(clippy::expect_used)]

use super::*;
use haider_core::SqliteStoreHandle;

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
