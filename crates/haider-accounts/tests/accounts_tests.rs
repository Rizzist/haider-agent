#![allow(clippy::unwrap_used)] // Test failures should stop at the asserted boundary.

#[cfg(unix)]
use std::ffi::OsString;
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
#[cfg(unix)]
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(target_os = "macos")]
use haider_accounts::KeychainVault;
use haider_accounts::{
    AccountIdentity, AccountStore, AccountsResult, AuthMethod, CredentialAlias,
    CredentialDescriptor, CredentialStatus, ErrorCode, JsonFileStore, MemoryVault, Resolver,
    RotationCallback, RotationDecision, RotationTrigger, StoreLike, Vault, import_env,
};
use haider_protocol::credential::RotationCause;

#[cfg(unix)]
const NOT_UNICODE_HELPER_FLAG: &str = "HAIDER_ACCOUNTS_NOT_UNICODE_HELPER";
#[cfg(unix)]
const NOT_UNICODE_ENV_VAR: &str = "HAIDER_ACCOUNTS_NOT_UNICODE_VALUE";
#[cfg(unix)]
const NOT_UNICODE_SECRET: &[u8] = &[0x66, 0x6f, 0xff];

fn descriptor(
    alias: &str,
    provider: &str,
    status: CredentialStatus,
    active: bool,
) -> CredentialDescriptor {
    CredentialDescriptor {
        alias: CredentialAlias::new(alias),
        provider: provider.to_owned(),
        base_url: None,
        auth_method: AuthMethod::ApiKey,
        identity: format!("{alias}@example.test"),
        status,
        active,
        label: None,
        account_identity: None,
        created_at_ms: None,
    }
}

#[test]
fn memory_vault_alias_round_trip_and_delete_are_stable() {
    let vault = MemoryVault::new();
    let beta = CredentialAlias::new("beta");
    let alpha = CredentialAlias::new("alpha");

    vault.put(&beta, b"beta-secret").unwrap();
    vault.put(&alpha, b"alpha-secret").unwrap();

    assert_eq!(
        vault.list().unwrap(),
        vec![CredentialAlias::new("alpha"), CredentialAlias::new("beta")]
    );
    assert_eq!(
        vault.resolve(&alpha).unwrap().expose_secret(),
        b"alpha-secret"
    );

    vault.delete(&alpha).unwrap();
    vault.delete(&alpha).unwrap();
    let error = vault.resolve(&alpha).unwrap_err();
    assert_eq!(error.code, ErrorCode::CredentialMissing);
}

#[test]
fn secret_handle_debug_and_display_are_redacted() {
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("redaction");
    let secret = "never-print-this-token";
    vault.put(&alias, secret.as_bytes()).unwrap();

    let handle = vault.resolve(&alias).unwrap();
    let debug = format!("{handle:?}");
    let display = format!("{handle}");

    assert_eq!(debug, "SecretHandle([REDACTED])");
    assert_eq!(display, "[REDACTED]");
    assert!(!debug.contains(secret));
    assert!(!display.contains(secret));
}

#[test]
fn account_store_enforces_one_active_account_per_provider() {
    let directory = tempfile::tempdir().unwrap();
    let store = JsonFileStore::new(directory.path());
    let mut accounts = AccountStore::new(store.clone()).unwrap();

    accounts
        .add(descriptor("first", "openai", CredentialStatus::Ok, false))
        .unwrap();
    accounts
        .add(descriptor("second", "openai", CredentialStatus::Ok, false))
        .unwrap();
    assert_active(&accounts, "openai", "first");

    accounts
        .add(descriptor("third", "openai", CredentialStatus::Ok, true))
        .unwrap();
    assert_active(&accounts, "openai", "third");
    assert_eq!(
        accounts
            .list()
            .iter()
            .filter(|account| account.provider == "openai" && account.active)
            .count(),
        1
    );

    accounts.select(&CredentialAlias::new("second")).unwrap();
    assert_active(&accounts, "openai", "second");
    accounts.remove(&CredentialAlias::new("second")).unwrap();
    assert_active(&accounts, "openai", "first");

    let reloaded = AccountStore::new(store).unwrap();
    assert_active(&reloaded, "openai", "first");
    assert_eq!(reloaded.list().len(), 2);
}

#[test]
fn new_accounts_are_timestamped_while_legacy_rows_stay_unknown() {
    let legacy = descriptor("legacy", "openai", CredentialStatus::Ok, true);
    let mut accounts = AccountStore::new(SnapshotStore::with_descriptors(vec![legacy])).unwrap();
    assert_eq!(accounts.list()[0].created_at_ms, None);
    accounts
        .backfill_identity(
            &CredentialAlias::new("legacy"),
            AccountIdentity {
                email: Some("owner@example.test".into()),
                display_name: None,
                account_id: None,
                plan: Some("pro".into()),
                issuer: None,
                captured_at: 964,
                verified: false,
            },
        )
        .unwrap();
    assert_eq!(accounts.list()[0].created_at_ms, None);

    accounts
        .add(descriptor("fresh", "openai", CredentialStatus::Ok, false))
        .unwrap();
    let created = accounts
        .get(&CredentialAlias::new("fresh"))
        .and_then(|descriptor| descriptor.created_at_ms);
    assert!(created.is_some());
    accounts
        .replace(descriptor("fresh", "openai", CredentialStatus::Ok, false))
        .unwrap();
    assert_eq!(
        accounts
            .get(&CredentialAlias::new("fresh"))
            .and_then(|descriptor| descriptor.created_at_ms),
        created
    );
}

#[test]
fn account_store_rejects_corrupt_active_invariant_on_load() {
    let store = SnapshotStore::with_descriptors(vec![
        descriptor("one", "anthropic", CredentialStatus::Ok, true),
        descriptor("two", "anthropic", CredentialStatus::Ok, true),
    ]);

    let error = AccountStore::new(store).err().unwrap();
    assert_eq!(error.code, ErrorCode::StoreCorrupt);
}

#[test]
fn failed_persistence_does_not_mutate_account_view() {
    let store = FailingSaveStore;
    let mut accounts = AccountStore::new(store).unwrap();

    let error = accounts
        .add(descriptor("one", "openai", CredentialStatus::Ok, true))
        .unwrap_err();

    assert_eq!(error.code, ErrorCode::StoreLocked);
    assert!(accounts.list().is_empty());
}

#[test]
fn resolver_picks_the_active_account() {
    let mut accounts = AccountStore::new(SnapshotStore::default()).unwrap();
    accounts
        .add(descriptor(
            "inactive",
            "openai",
            CredentialStatus::Ok,
            false,
        ))
        .unwrap();
    accounts
        .add(descriptor("active", "openai", CredentialStatus::Ok, true))
        .unwrap();
    let vault = MemoryVault::new();
    vault
        .put(&CredentialAlias::new("inactive"), b"inactive-secret")
        .unwrap();
    vault
        .put(&CredentialAlias::new("active"), b"active-secret")
        .unwrap();
    let callback = FixedRotation::new(RotationDecision::Stop);
    let resolver = Resolver::new(&accounts, &vault, &callback);

    let (selected, secret) = resolver.resolve_for_provider("openai").unwrap();

    assert_eq!(selected.alias, CredentialAlias::new("active"));
    assert_eq!(secret.expose_secret(), b"active-secret");
    assert_eq!(callback.calls(), 0);
}

#[test]
fn expired_limit_is_skipped_without_invoking_rotation_callback() {
    let mut accounts = AccountStore::new(SnapshotStore::default()).unwrap();
    accounts
        .add(descriptor(
            "stale-limit",
            "openai",
            CredentialStatus::Limited { until_ms: 0 },
            true,
        ))
        .unwrap();
    let vault = MemoryVault::new();
    vault
        .put(&CredentialAlias::new("stale-limit"), b"usable-again")
        .unwrap();
    let callback = FixedRotation::new(RotationDecision::Stop);
    let resolver = Resolver::new(&accounts, &vault, &callback);

    let (selected, secret) = resolver.resolve_for_provider("openai").unwrap();

    assert_eq!(selected.alias, CredentialAlias::new("stale-limit"));
    assert_eq!(secret.expose_secret(), b"usable-again");
    assert_eq!(callback.calls(), 0);
}

#[test]
fn current_limit_delegates_rotation_decision_to_callback() {
    let mut accounts = AccountStore::new(SnapshotStore::default()).unwrap();
    accounts
        .add(descriptor(
            "limited",
            "openai",
            CredentialStatus::Limited { until_ms: u64::MAX },
            true,
        ))
        .unwrap();
    accounts
        .add(descriptor(
            "alternate",
            "openai",
            CredentialStatus::Ok,
            false,
        ))
        .unwrap();
    let vault = MemoryVault::new();
    vault
        .put(&CredentialAlias::new("alternate"), b"alternate-secret")
        .unwrap();
    let callback = FixedRotation::new(RotationDecision::RotateTo(CredentialAlias::new(
        "alternate",
    )));
    let resolver = Resolver::new(&accounts, &vault, &callback);

    let (selected, secret) = resolver.resolve_for_provider("openai").unwrap();

    assert_eq!(selected.alias, CredentialAlias::new("alternate"));
    assert_eq!(secret.expose_secret(), b"alternate-secret");
    assert_eq!(callback.calls(), 1);
}

#[test]
fn rotation_target_that_is_also_limited_stops_after_one_callback() {
    let mut accounts = AccountStore::new(SnapshotStore::default()).unwrap();
    accounts
        .add(descriptor(
            "limited-active",
            "openai",
            CredentialStatus::Limited { until_ms: u64::MAX },
            true,
        ))
        .unwrap();
    accounts
        .add(descriptor(
            "limited-target",
            "openai",
            CredentialStatus::Limited { until_ms: u64::MAX },
            false,
        ))
        .unwrap();
    let vault = MemoryVault::new();
    let callback = FixedRotation::new(RotationDecision::RotateTo(CredentialAlias::new(
        "limited-target",
    )));
    let resolver = Resolver::new(&accounts, &vault, &callback);

    let error = resolver.resolve_for_provider("openai").unwrap_err();

    assert_eq!(error.code, ErrorCode::CredentialLimited);
    assert!(error.retryable);
    assert!(error.message.contains("limited-target"));
    assert_eq!(callback.calls(), 1);
}

/// MUTATION CHECK (W5c.1 typed alternate seam): restore the old direct
/// `Expired` rejection or map either authentication trigger to a rate-limit
/// cause/deadline. Expected failure: the checked alternate is not selected,
/// the callback count changes, or the cause/status assertions fail.
/// Verified by revert on 2026-07-29.
#[test]
fn authentication_triggers_use_one_checked_error_rotation_without_a_fake_deadline() {
    let mut accounts = AccountStore::new(SnapshotStore::default()).unwrap();
    accounts
        .add(descriptor(
            "expired-active",
            "openai",
            CredentialStatus::Expired,
            true,
        ))
        .unwrap();
    accounts
        .add(descriptor(
            "usable-alternate",
            "openai",
            CredentialStatus::Ok,
            false,
        ))
        .unwrap();
    let vault = MemoryVault::new();
    vault
        .put(
            &CredentialAlias::new("usable-alternate"),
            b"alternate-secret",
        )
        .unwrap();
    let callback = FixedRotation::new(RotationDecision::RotateTo(CredentialAlias::new(
        "usable-alternate",
    )));
    let resolver = Resolver::new(&accounts, &vault, &callback);

    let (selected, secret) = resolver.resolve_for_provider("openai").unwrap();

    assert_eq!(selected.alias, CredentialAlias::new("usable-alternate"));
    assert_eq!(secret.expose_secret(), b"alternate-secret");
    assert_eq!(callback.calls(), 1, "policy is invoked exactly once");
    assert_eq!(RotationTrigger::AuthExpired.cause(), RotationCause::Error);
    assert_eq!(RotationTrigger::RefreshFailed.cause(), RotationCause::Error);
    assert!(
        matches!(
            accounts
                .get(&CredentialAlias::new("expired-active"))
                .map(|descriptor| &descriptor.status),
            Some(CredentialStatus::Expired)
        ),
        "authentication failure must not invent a Limited deadline"
    );
}

#[test]
fn wait_and_stop_decisions_preserve_retryability() {
    for (decision, expected_retryable) in [
        (RotationDecision::Wait, true),
        (RotationDecision::Stop, false),
    ] {
        let mut accounts = AccountStore::new(SnapshotStore::default()).unwrap();
        accounts
            .add(descriptor(
                "limited",
                "openai",
                CredentialStatus::Limited { until_ms: u64::MAX },
                true,
            ))
            .unwrap();
        let vault = MemoryVault::new();
        let callback = FixedRotation::new(decision);
        let resolver = Resolver::new(&accounts, &vault, &callback);

        let error = resolver.resolve_for_provider("openai").unwrap_err();

        assert_eq!(error.code, ErrorCode::CredentialLimited);
        assert_eq!(error.retryable, expected_retryable);
    }
}

#[test]
fn env_import_reads_and_vaults_a_single_environment_value() {
    let vault = MemoryVault::new();
    let expected = std::env::var("PATH").unwrap();

    let alias = import_env(&vault, "migration-test", "PATH").unwrap();

    assert_eq!(alias, CredentialAlias::new("migration-test-env"));
    assert_eq!(
        vault.resolve(&alias).unwrap().expose_secret(),
        expected.as_bytes()
    );
}

#[test]
fn env_import_of_unset_variable_reports_credential_missing() {
    let vault = MemoryVault::new();

    let error = import_env(&vault, "migration-test", "HAIDER_ACCOUNTS_TEST_UNSET_VAR").unwrap_err();

    assert_eq!(error.code, ErrorCode::CredentialMissing);
    assert!(error.message.contains("HAIDER_ACCOUNTS_TEST_UNSET_VAR"));
}

#[cfg(unix)]
#[test]
fn env_import_of_not_unicode_value_never_exposes_secret() {
    if std::env::var_os(NOT_UNICODE_HELPER_FLAG).is_some() {
        run_not_unicode_import_helper();
        return;
    }

    let output = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("env_import_of_not_unicode_value_never_exposes_secret")
        .arg("--nocapture")
        .env(NOT_UNICODE_HELPER_FLAG, "1")
        .env(
            NOT_UNICODE_ENV_VAR,
            OsString::from_vec(NOT_UNICODE_SECRET.to_vec()),
        )
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_not_unicode_secret_absent(&output.stdout);
    assert_not_unicode_secret_absent(&output.stderr);
}

#[cfg(unix)]
fn run_not_unicode_import_helper() {
    let vault = MemoryVault::new();
    let error = import_env(&vault, "not-unicode-test", NOT_UNICODE_ENV_VAR).unwrap_err();
    let debug = format!("{error:?}");
    let serialized = serde_json::to_vec(&error).unwrap();

    assert_eq!(error.code, ErrorCode::CredentialMissing);
    assert!(!error.retryable);
    assert_not_unicode_secret_absent(error.message.as_bytes());
    assert_not_unicode_secret_absent(debug.as_bytes());
    assert_not_unicode_secret_absent(&serialized);

    println!("message={}", error.message);
    println!("debug={debug}");
    println!("serialized={}", String::from_utf8(serialized).unwrap());
}

#[test]
fn json_file_store_uses_profile_relative_accounts_file() {
    let directory = tempfile::tempdir().unwrap();
    let store = JsonFileStore::new(directory.path());
    let mut accounts = AccountStore::new(store.clone()).unwrap();
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("persisted");
    let vaulted_secret = b"vaulted-json-sentinel-7f4b3e9102";

    vault.put(&alias, vaulted_secret).unwrap();
    assert_eq!(
        vault.resolve(&alias).unwrap().expose_secret(),
        vaulted_secret
    );

    accounts
        .add(descriptor(
            "persisted",
            "openai",
            CredentialStatus::Ok,
            true,
        ))
        .unwrap();

    assert_eq!(store.path(), directory.path().join("accounts.json"));
    let json = std::fs::read(store.path()).unwrap();
    assert!(contains_bytes(&json, b"\"alias\": \"persisted\""));
    assert_bytes_absent(&json, vaulted_secret);
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires an interactive macOS Keychain; headless CI has no Keychain UI"]
fn keychain_round_trip() {
    use std::time::{SystemTime, UNIX_EPOCH};

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let alias = CredentialAlias::new(format!(
        "haider-accounts-test-{}-{unique}",
        std::process::id()
    ));
    let vault = KeychainVault::new();
    let secret = b"non-production-keychain-test-secret";

    vault.put(&alias, secret).unwrap();
    assert_eq!(vault.resolve(&alias).unwrap().expose_secret(), secret);
    assert!(vault.list().unwrap().contains(&alias));
    vault.delete(&alias).unwrap();
    assert_eq!(
        vault.resolve(&alias).unwrap_err().code,
        ErrorCode::CredentialMissing
    );
}

fn assert_active<S: StoreLike>(accounts: &AccountStore<S>, provider: &str, alias: &str) {
    assert_eq!(
        accounts
            .active_for_provider(provider)
            .map(|account| account.alias.as_str()),
        Some(alias)
    );
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn assert_bytes_absent(haystack: &[u8], needle: &[u8]) {
    assert!(!contains_bytes(haystack, needle));
}

#[cfg(unix)]
fn assert_not_unicode_secret_absent(bytes: &[u8]) {
    // Cover raw propagation, OsString's escaped Debug/Display form, and lossy
    // UTF-8 conversion. A regression must not become invisible merely because
    // Rust renders the invalid byte instead of writing it verbatim.
    for representation in [
        NOT_UNICODE_SECRET,
        b"fo\\xFF",
        b"fo\\xff",
        b"fo\xef\xbf\xbd",
    ] {
        assert_bytes_absent(bytes, representation);
    }
}

#[derive(Default)]
struct SnapshotStore {
    descriptors: Mutex<Vec<CredentialDescriptor>>,
}

impl SnapshotStore {
    fn with_descriptors(descriptors: Vec<CredentialDescriptor>) -> Self {
        Self {
            descriptors: Mutex::new(descriptors),
        }
    }
}

impl StoreLike for SnapshotStore {
    fn load(&self) -> AccountsResult<Vec<CredentialDescriptor>> {
        Ok(self.descriptors.lock().unwrap().clone())
    }

    fn save(&self, descriptors: &[CredentialDescriptor]) -> AccountsResult<()> {
        *self.descriptors.lock().unwrap() = descriptors.to_vec();
        Ok(())
    }
}

/// Store double whose `save` always fails, for no-mutation-on-error tests.
struct FailingSaveStore;

impl StoreLike for FailingSaveStore {
    fn load(&self) -> AccountsResult<Vec<CredentialDescriptor>> {
        Ok(Vec::new())
    }

    fn save(&self, _descriptors: &[CredentialDescriptor]) -> AccountsResult<()> {
        Err(haider_accounts::HaiderError::new(
            ErrorCode::StoreLocked,
            "test double rejected save",
            true,
        ))
    }
}

struct FixedRotation {
    decision: RotationDecision,
    calls: AtomicUsize,
}

impl FixedRotation {
    fn new(decision: RotationDecision) -> Self {
        Self {
            decision,
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl RotationCallback for FixedRotation {
    fn on_limited(&self, _alias: &CredentialAlias, _until_ms: u64) -> RotationDecision {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.decision.clone()
    }

    fn on_rotation(&self, _alias: &CredentialAlias, _trigger: RotationTrigger) -> RotationDecision {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.decision.clone()
    }
}

/// v0.0.938 account labels: `set_label` sets and CLEARS the operator's
/// display name, while `replace` — the rotation/re-login path — preserves an
/// existing label when the rebuilt descriptor carries none. The two intents
/// are separate methods precisely so absence can mean "keep it" in one and
/// "clear it" in the other without ambiguity.
///
/// MUTATION CHECK (executed): make `replace` overwrite the label
/// unconditionally (drop the is_none guard) and the survives-rotation half
/// fails — every re-authentication would silently discard the chosen name.
#[test]
fn labels_are_set_cleared_and_survive_rotation() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = AccountStore::new(JsonFileStore::new(directory.path())).unwrap();
    let alias = CredentialAlias::new("label-acct");
    store
        .add(descriptor("label-acct", "acme", CredentialStatus::Ok, true))
        .unwrap();

    // Absent by default — a surface falls back to identity, then alias.
    assert_eq!(store.list()[0].label, None);

    let updated = store
        .set_label(&alias, Some("work openai".to_owned()))
        .unwrap();
    assert_eq!(updated.label.as_deref(), Some("work openai"));
    assert_eq!(store.list()[0].label.as_deref(), Some("work openai"));

    // Rotation/re-login rebuilds the descriptor from provider truth with no
    // label; the operator's name must survive.
    let mut rebuilt = descriptor("label-acct", "acme", CredentialStatus::Ok, true);
    rebuilt.identity = "rotated@example.test".to_owned();
    rebuilt.label = None;
    store.replace(rebuilt).unwrap();
    assert_eq!(
        store.list()[0].label.as_deref(),
        Some("work openai"),
        "re-authentication must not discard the chosen name"
    );
    assert_eq!(store.list()[0].identity, "rotated@example.test");

    // An incoming label still wins on replace.
    let mut renamed = descriptor("label-acct", "acme", CredentialStatus::Ok, true);
    renamed.label = Some("renamed by rebuild".to_owned());
    store.replace(renamed).unwrap();
    assert_eq!(store.list()[0].label.as_deref(), Some("renamed by rebuild"));

    // And the label door clears it — absence means clear HERE.
    let cleared = store.set_label(&alias, None).unwrap();
    assert_eq!(cleared.label, None);
    assert_eq!(store.list()[0].label, None);

    // An unknown alias is an honest miss, never a silent no-op.
    assert!(
        store
            .set_label(&CredentialAlias::new("nope"), Some("x".into()))
            .is_err()
    );
}
