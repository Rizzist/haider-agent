//! W3c2 account/vault acceptance over the REAL production runtime and a
//! real UnixStream (report §6.2): staged login success and replay, 401 vs
//! 403, retryable validation + restage, crash-boundary reconciliation, the
//! vault-unsupported gate, and the sentinel-secret sweep.

mod support;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use haider_accounts::{MemoryVault, StoreLike, Vault};
use haider_daemon::{
    AccountsDependencies, CredentialValidator, DaemonConfig, DaemonDependencies, ValidatedIdentity,
    ValidationError, ValidationFailureKind, VaultProvision,
};
use haider_protocol::credential::CredentialDescriptor;
use haider_protocol::error::HaiderError;
use haider_protocol::ids::CredentialAlias;
use haider_rpc::{
    ClientKind, CommandId, DEFAULT_FRAME_LIMIT, ERROR_CODE_PERMISSION_DENIED,
    ERROR_CODE_PROVIDER_ERROR, ERROR_CODE_RESTAGE_REQUIRED, ERROR_CODE_UNAUTHORIZED,
    ERROR_CODE_VAULT_UNSUPPORTED, RequestBody, RequestId, ResponseBody, SecretWire, StagePurpose,
    WireFrame,
};
use support::{DEADLINE, UdsClient, ready_with_dependencies, test_root};

const LIMIT: usize = DEFAULT_FRAME_LIMIT;

// ───────────────────────────── injectable fakes ─────────────────────────────

/// Scripted validator: pops one scripted outcome per call; an empty script
/// succeeds. Never touches the network.
struct ScriptedValidator {
    script: StdMutex<VecDeque<Result<ValidatedIdentity, ValidationError>>>,
    calls: AtomicUsize,
    /// Secrets observed by validation calls, for sweep assertions only —
    /// this fake deliberately COPIES what production must never persist.
    observed: StdMutex<Vec<Vec<u8>>>,
}

impl ScriptedValidator {
    fn new(script: Vec<Result<ValidatedIdentity, ValidationError>>) -> Arc<Self> {
        Arc::new(Self {
            script: StdMutex::new(script.into()),
            calls: AtomicUsize::new(0),
            observed: StdMutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl CredentialValidator for ScriptedValidator {
    fn supports(&self, provider: &str) -> bool {
        provider == "anthropic"
    }

    async fn validate(
        &self,
        _provider: &str,
        _model: &str,
        secret: &[u8],
    ) -> Result<ValidatedIdentity, ValidationError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut observed) = self.observed.lock() {
            observed.push(secret.to_vec());
        }
        match self.script.lock().ok().and_then(|mut s| s.pop_front()) {
            Some(outcome) => outcome,
            None => Ok(ValidatedIdentity {
                identity: "scripted identity".into(),
            }),
        }
    }
}

fn unauthorized() -> Result<ValidatedIdentity, ValidationError> {
    Err(ValidationError {
        kind: ValidationFailureKind::Unauthorized,
        message: "credential validation reported Authentication".into(),
    })
}

fn permission_denied() -> Result<ValidatedIdentity, ValidationError> {
    Err(ValidationError {
        kind: ValidationFailureKind::PermissionDenied,
        message: "credential validation reported PermissionDenied".into(),
    })
}

fn unavailable() -> Result<ValidatedIdentity, ValidationError> {
    Err(ValidationError {
        kind: ValidationFailureKind::Unavailable,
        message: "credential validation reported Overloaded".into(),
    })
}

/// Shared in-memory descriptor store surviving in-process daemon restarts.
#[derive(Default)]
struct SharedDescriptors {
    rows: StdMutex<Vec<CredentialDescriptor>>,
}

impl SharedDescriptors {
    fn wipe(&self) {
        if let Ok(mut rows) = self.rows.lock() {
            rows.clear();
        }
    }

    fn dump(&self) -> Vec<CredentialDescriptor> {
        self.rows
            .lock()
            .map(|rows| rows.clone())
            .unwrap_or_default()
    }
}

impl StoreLike for SharedDescriptors {
    fn load(&self) -> Result<Vec<CredentialDescriptor>, HaiderError> {
        Ok(self.dump())
    }

    fn save(&self, descriptors: &[CredentialDescriptor]) -> Result<(), HaiderError> {
        if let Ok(mut rows) = self.rows.lock() {
            *rows = descriptors.to_vec();
        }
        Ok(())
    }
}

struct AccountFixture {
    vault: Arc<MemoryVault>,
    validator: Arc<ScriptedValidator>,
    descriptors: Arc<SharedDescriptors>,
}

impl AccountFixture {
    fn new(script: Vec<Result<ValidatedIdentity, ValidationError>>) -> Self {
        Self {
            vault: Arc::new(MemoryVault::default()),
            validator: ScriptedValidator::new(script),
            descriptors: Arc::new(SharedDescriptors::default()),
        }
    }

    fn dependencies(&self) -> DaemonDependencies {
        DaemonDependencies {
            accounts: AccountsDependencies {
                vault: VaultProvision::Available(self.vault.clone() as Arc<dyn Vault>),
                validator: self.validator.clone(),
                descriptor_store: Some(self.descriptors.clone() as Arc<dyn StoreLike>),
            },
            ..DaemonDependencies::default()
        }
    }
}

// ───────────────────────────── wire helpers ────────────────────────────────

async fn request(client: &mut UdsClient, id: &str, body: RequestBody) -> ResponseBody {
    client
        .send(
            &WireFrame::Request {
                request_id: RequestId::new(id),
                body,
            },
            LIMIT,
        )
        .await;
    response_for(client, id).await
}

async fn response_for(client: &mut UdsClient, id: &str) -> ResponseBody {
    tokio::time::timeout(DEADLINE, async {
        loop {
            if let WireFrame::Response { request_id, body } = client.receive().await
                && request_id.as_str() == id
            {
                return body;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no response for request {id}"))
}

async fn stage_secret(client: &mut UdsClient, stage_id: &str, secret: &str) -> String {
    let body = request(
        client,
        &format!("req-stage-{stage_id}"),
        RequestBody::VaultStage {
            stage_id: stage_id.into(),
            purpose: StagePurpose::ApiKey,
            secret: SecretWire::new(secret),
        },
    )
    .await;
    match body {
        ResponseBody::VaultStage {
            vault_reference,
            expires_at_ms,
            ..
        } => {
            assert!(expires_at_ms > 0);
            vault_reference
        }
        other => panic!("expected vault.stage response, got {other:?}"),
    }
}

fn login_body(command: &str, reference: &str, alias: Option<&str>) -> RequestBody {
    RequestBody::AccountLoginApi {
        command_id: CommandId::new(command),
        provider: "anthropic".into(),
        alias: alias.map(str::to_owned),
        vault_reference: reference.into(),
        validation_model: None,
    }
}

fn expect_descriptor(body: ResponseBody) -> CredentialDescriptor {
    match body {
        ResponseBody::AccountLoginApi { descriptor } => descriptor,
        other => panic!("expected login response, got {other:?}"),
    }
}

fn expect_error(body: ResponseBody) -> (String, bool) {
    match body {
        ResponseBody::Error {
            code, retryable, ..
        } => (code, retryable),
        other => panic!("expected error response, got {other:?}"),
    }
}

async fn control_client(config: &DaemonConfig) -> UdsClient {
    UdsClient::connect_control(
        &config.endpoint_path(),
        LIMIT,
        "haider-account-tests",
        "account-client-1",
        ClientKind::Cli,
    )
    .await
}

// ────────────────────────────────── tests ───────────────────────────────────

// MUTATION CHECK (R10 steps 4-9 + R7 lost-response retry): break the actor's
// commit order (finalize before descriptor add) or make the receipt include
// the vault reference. Expected failure: the replay assertions below (same
// descriptor from a NEW stage under the SAME command) or the identity check
// fail.
#[tokio::test]
async fn staged_login_commits_descriptor_lists_it_and_replays_for_lost_responses() {
    let root = test_root("hacL");
    let config = DaemonConfig::new("profile-login", root.path().join("store"), root.path());
    let fixture = AccountFixture::new(Vec::new());
    let task = ready_with_dependencies(&config, fixture.dependencies()).await;
    let mut client = control_client(&config).await;

    let reference = stage_secret(&mut client, "stage-1", "sk-live-login-1").await;
    let descriptor = expect_descriptor(
        request(
            &mut client,
            "req-login-1",
            login_body("command-login-1", &reference, Some("work")),
        )
        .await,
    );
    assert_eq!(descriptor.provider, "anthropic");
    assert_eq!(descriptor.identity, "work");
    assert!(descriptor.active);
    assert!(
        descriptor.alias.as_str().starts_with("anthropic-"),
        "physical alias is the vault identity: {}",
        descriptor.alias
    );
    // The vault holds the secret under the physical alias.
    let resolved = fixture
        .vault
        .resolve(&CredentialAlias::new(descriptor.alias.as_str()))
        .unwrap_or_else(|error| panic!("{error:?}"));
    assert_eq!(resolved.expose_secret(), b"sk-live-login-1");

    // account.list serves the committed descriptor (View surface).
    let listed = request(
        &mut client,
        "req-list-1",
        RequestBody::AccountList {
            provider: Some("anthropic".into()),
        },
    )
    .await;
    match listed {
        ResponseBody::AccountList { descriptors } => {
            assert_eq!(descriptors.len(), 1);
            assert_eq!(descriptors[0], descriptor);
        }
        other => panic!("expected account.list response, got {other:?}"),
    }

    // Lost-response retry: a NEW stage under the SAME command id replays the
    // committed result — and does not validate again.
    let calls_before = fixture.validator.calls();
    let fresh_reference = stage_secret(&mut client, "stage-2", "sk-live-login-1-retyped").await;
    let replayed = expect_descriptor(
        request(
            &mut client,
            "req-login-1-retry",
            login_body("command-login-1", &fresh_reference, Some("work")),
        )
        .await,
    );
    assert_eq!(replayed, descriptor);
    assert_eq!(
        fixture.validator.calls(),
        calls_before,
        "a committed replay must not re-validate"
    );

    // Same command id with a DIFFERENT semantic body is rejected.
    let reference_3 = stage_secret(&mut client, "stage-3", "sk-live-login-1").await;
    let (code, _) = expect_error(
        request(
            &mut client,
            "req-login-1-mismatch",
            login_body("command-login-1", &reference_3, Some("other-alias")),
        )
        .await,
    );
    assert_eq!(code, "invalid_argument");

    task.shutdown_handle().request("test complete");
    let _ = task.join().await;
}

// MUTATION CHECK (R10 step 8): make the actor treat Unauthorized as
// retryable (keep the pending secret / skip fail_login_receipt). Expected
// failure: the nothing-persisted assertions and the terminal-replay
// assertion below.
#[tokio::test]
async fn unauthorized_and_permission_denied_are_definitive_and_persist_nothing() {
    let root = test_root("hac4");
    let config = DaemonConfig::new("profile-4xx", root.path().join("store"), root.path());
    let fixture = AccountFixture::new(vec![unauthorized(), permission_denied()]);
    let task = ready_with_dependencies(&config, fixture.dependencies()).await;
    let mut client = control_client(&config).await;

    // 401: invalid key.
    let reference = stage_secret(&mut client, "stage-401", "sk-invalid").await;
    let (code, retryable) = expect_error(
        request(
            &mut client,
            "req-401",
            login_body("command-401", &reference, None),
        )
        .await,
    );
    assert_eq!(code, ERROR_CODE_UNAUTHORIZED);
    assert!(!retryable);

    // 403: authenticated but not permitted for the model/endpoint.
    let reference = stage_secret(&mut client, "stage-403", "sk-permitted-not").await;
    let (code, retryable) = expect_error(
        request(
            &mut client,
            "req-403",
            login_body("command-403", &reference, None),
        )
        .await,
    );
    assert_eq!(code, ERROR_CODE_PERMISSION_DENIED);
    assert!(!retryable);

    // Nothing persistent was written on either definitive failure.
    assert!(fixture.descriptors.dump().is_empty(), "no descriptors");
    assert!(
        fixture
            .vault
            .list()
            .unwrap_or_else(|error| panic!("{error:?}"))
            .is_empty(),
        "no vault entries"
    );

    // The definitive failure is terminal: a retry with a FRESH stage under
    // the same command id is a typed rejection, not a new validation.
    let calls_before = fixture.validator.calls();
    let reference = stage_secret(&mut client, "stage-401-retry", "sk-invalid").await;
    let (code, retryable) = expect_error(
        request(
            &mut client,
            "req-401-retry",
            login_body("command-401", &reference, None),
        )
        .await,
    );
    assert_eq!(code, "invalid_argument");
    assert!(!retryable);
    assert_eq!(fixture.validator.calls(), calls_before);

    task.shutdown_handle().request("test complete");
    let _ = task.join().await;
}

// MUTATION CHECK (R10 command-owned secret): make the actor wipe the secret
// on a RETRYABLE validation failure. Expected failure: the stage-less retry
// below answers restage_required instead of succeeding.
#[tokio::test]
async fn retryable_validation_keeps_the_pending_command_for_a_stageless_retry() {
    let root = test_root("hacR");
    let config = DaemonConfig::new("profile-retry", root.path().join("store"), root.path());
    let fixture = AccountFixture::new(vec![unavailable()]);
    let task = ready_with_dependencies(&config, fixture.dependencies()).await;
    let mut client = control_client(&config).await;

    let reference = stage_secret(&mut client, "stage-retry", "sk-eventually-good").await;
    let (code, retryable) = expect_error(
        request(
            &mut client,
            "req-retry-1",
            login_body("command-retry", &reference, None),
        )
        .await,
    );
    assert_eq!(code, ERROR_CODE_PROVIDER_ERROR);
    assert!(retryable, "validation unavailability must be retryable");

    // Retry the SAME command with the SAME (now consumed) reference: the
    // command still owns the secret, so no restage is needed.
    let descriptor = expect_descriptor(
        request(
            &mut client,
            "req-retry-2",
            login_body("command-retry", &reference, None),
        )
        .await,
    );
    assert_eq!(fixture.validator.calls(), 2);
    let resolved = fixture
        .vault
        .resolve(&CredentialAlias::new(descriptor.alias.as_str()))
        .unwrap_or_else(|error| panic!("{error:?}"));
    assert_eq!(resolved.expose_secret(), b"sk-eventually-good");

    task.shutdown_handle().request("test complete");
    let _ = task.join().await;
}

// MUTATION CHECK (R10 restart wipe + reconciliation): make the daemon
// resurrect pending secrets across restart (persist them anywhere) or make
// reconciliation fabricate a descriptor for a pending+neither receipt.
// Expected failure: the restage_required assertion below, or the
// post-restart list shows a descriptor before the fresh-stage retry.
#[tokio::test]
async fn restart_wipes_the_pending_secret_and_a_fresh_stage_completes_the_command() {
    let root = test_root("hacW");
    let config = DaemonConfig::new("profile-wipe", root.path().join("store"), root.path());
    let fixture = AccountFixture::new(vec![unavailable()]);
    let task = ready_with_dependencies(&config, fixture.dependencies()).await;
    let mut client = control_client(&config).await;

    let reference = stage_secret(&mut client, "stage-w", "sk-wiped-by-restart").await;
    let (code, _) = expect_error(
        request(
            &mut client,
            "req-w-1",
            login_body("command-wipe", &reference, None),
        )
        .await,
    );
    assert_eq!(code, ERROR_CODE_PROVIDER_ERROR);

    // Crash: the pending receipt is durable; the command-owned secret is not.
    task.crash().await;
    let task = ready_with_dependencies(&config, fixture.dependencies()).await;
    let mut client = control_client(&config).await;

    // Reconciliation left the receipt pending and produced NO descriptor.
    let listed = request(
        &mut client,
        "req-w-list",
        RequestBody::AccountList { provider: None },
    )
    .await;
    match listed {
        ResponseBody::AccountList { descriptors } => assert!(descriptors.is_empty()),
        other => panic!("expected account.list response, got {other:?}"),
    }

    // Same command, no stage: the daemon restart wiped the secret.
    let (code, retryable) = expect_error(
        request(
            &mut client,
            "req-w-2",
            login_body("command-wipe", &reference, None),
        )
        .await,
    );
    assert_eq!(code, ERROR_CODE_RESTAGE_REQUIRED);
    assert!(retryable);

    // A fresh stage completes the SAME durable command.
    let fresh = stage_secret(&mut client, "stage-w-2", "sk-wiped-by-restart").await;
    let descriptor = expect_descriptor(
        request(
            &mut client,
            "req-w-3",
            login_body("command-wipe", &fresh, None),
        )
        .await,
    );
    assert_eq!(descriptor.provider, "anthropic");

    task.shutdown_handle().request("test complete");
    let _ = task.join().await;
}

// MUTATION CHECK (R10 step 10 committed reconciliation): remove the
// committed-receipt self-heal arm. Expected failure: the post-restart list
// below is empty.
#[tokio::test]
async fn committed_receipt_self_heals_a_missing_descriptor_on_restart() {
    let root = test_root("hacH");
    let config = DaemonConfig::new("profile-heal", root.path().join("store"), root.path());
    let fixture = AccountFixture::new(Vec::new());
    let task = ready_with_dependencies(&config, fixture.dependencies()).await;
    let mut client = control_client(&config).await;

    let reference = stage_secret(&mut client, "stage-h", "sk-healed").await;
    let descriptor = expect_descriptor(
        request(
            &mut client,
            "req-h-1",
            login_body("command-heal", &reference, Some("healme")),
        )
        .await,
    );

    // Lose the descriptor file contents (rollback/tamper), keep the vault.
    task.crash().await;
    fixture.descriptors.wipe();
    let task = ready_with_dependencies(&config, fixture.dependencies()).await;
    let mut client = control_client(&config).await;

    let listed = request(
        &mut client,
        "req-h-list",
        RequestBody::AccountList { provider: None },
    )
    .await;
    match listed {
        ResponseBody::AccountList { descriptors } => {
            assert_eq!(descriptors.len(), 1, "reconciliation must self-heal");
            assert_eq!(descriptors[0].alias, descriptor.alias);
        }
        other => panic!("expected account.list response, got {other:?}"),
    }

    task.shutdown_handle().request("test complete");
    let _ = task.join().await;
}

// R10 platform gate: a vaultless daemon rejects staging AND login with the
// stable `vault_unsupported`, never a generic internal message; listing
// still serves (empty).
#[tokio::test]
async fn vaultless_platforms_answer_the_stable_vault_unsupported_code() {
    let root = test_root("hacU");
    let config = DaemonConfig::new("profile-novault", root.path().join("store"), root.path());
    let dependencies = DaemonDependencies {
        accounts: AccountsDependencies {
            vault: VaultProvision::Unsupported,
            ..AccountsDependencies::default()
        },
        ..DaemonDependencies::default()
    };
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = control_client(&config).await;

    let (code, retryable) = expect_error(
        request(
            &mut client,
            "req-nv-stage",
            RequestBody::VaultStage {
                stage_id: "stage-nv".into(),
                purpose: StagePurpose::ApiKey,
                secret: SecretWire::new("sk-nowhere-to-go"),
            },
        )
        .await,
    );
    assert_eq!(code, ERROR_CODE_VAULT_UNSUPPORTED);
    assert!(!retryable);

    let (code, _) = expect_error(
        request(
            &mut client,
            "req-nv-login",
            login_body("command-nv", "vaultref-none", None),
        )
        .await,
    );
    assert_eq!(code, ERROR_CODE_VAULT_UNSUPPORTED);

    let listed = request(
        &mut client,
        "req-nv-list",
        RequestBody::AccountList { provider: None },
    )
    .await;
    assert!(matches!(
        listed,
        ResponseBody::AccountList { descriptors } if descriptors.is_empty()
    ));

    task.shutdown_handle().request("test complete");
    let _ = task.join().await;
}

// THE SENTINEL-SECRET SWEEP (the brief's proof obligation): after staging,
// login, and a full daemon lifecycle, one unique key must be absent from the
// SQLite journal bytes (events + receipts), every file under the profile
// store (descriptor JSON included), and ordinary frame formatting.
//
// MUTATION CHECK: make the login receipt's request_json include the staged
// secret (or the descriptor's identity carry it). Expected failure: the
// byte-scan below finds the sentinel in store.sqlite/accounts.json.
#[tokio::test]
async fn sentinel_secret_is_absent_from_store_files_receipts_and_formatted_frames() {
    const SENTINEL: &str = "sk-sentinel-3f9a71c2d84e5b06-w3c2";

    let root = test_root("hacS");
    let store_dir = root.path().join("store");
    let config = DaemonConfig::new("profile-sentinel", store_dir.clone(), root.path());
    // Real JSON descriptor store on disk (descriptor_store: None) so the
    // sweep covers the production accounts.json bytes; vault stays in
    // memory (the Keychain is out of scope for a unit sweep).
    let vault = Arc::new(MemoryVault::default());
    let validator = ScriptedValidator::new(Vec::new());
    let dependencies = DaemonDependencies {
        accounts: AccountsDependencies {
            vault: VaultProvision::Available(vault.clone() as Arc<dyn Vault>),
            validator: validator.clone(),
            descriptor_store: None,
        },
        ..DaemonDependencies::default()
    };
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = control_client(&config).await;

    // Ordinary frame formatting never reveals the secret (redaction law).
    let stage_frame = WireFrame::Request {
        request_id: RequestId::new("req-sent-stage"),
        body: RequestBody::VaultStage {
            stage_id: "stage-sentinel".into(),
            purpose: StagePurpose::ApiKey,
            secret: SecretWire::new(SENTINEL),
        },
    };
    let formatted = format!("{stage_frame:?}");
    assert!(
        !formatted.contains(SENTINEL) && formatted.contains("[REDACTED]"),
        "frame Debug must redact the staged secret"
    );
    client.send(&stage_frame, LIMIT).await;
    let reference = match response_for(&mut client, "req-sent-stage").await {
        ResponseBody::VaultStage {
            vault_reference, ..
        } => vault_reference,
        other => panic!("expected vault.stage response, got {other:?}"),
    };
    let descriptor = expect_descriptor(
        request(
            &mut client,
            "req-sent-login",
            login_body("command-sentinel", &reference, Some("sweep")),
        )
        .await,
    );
    let descriptor_json =
        serde_json::to_string(&descriptor).unwrap_or_else(|error| panic!("{error}"));
    assert!(!descriptor_json.contains(SENTINEL));

    // Graceful drain flushes and closes the store, then sweep the bytes of
    // EVERY file under the profile store (SQLite journal incl. receipts,
    // accounts.json, WAL leftovers).
    task.shutdown_handle().request("sweep");
    let _ = task.join().await;
    let mut scanned = 0_usize;
    let sentinel_bytes = SENTINEL.as_bytes();
    let mut stack = vec![store_dir.clone()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).unwrap_or_else(|error| panic!("read_dir: {error}"));
        for entry in entries {
            let path = entry
                .unwrap_or_else(|error| panic!("entry: {error}"))
                .path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let bytes = std::fs::read(&path).unwrap_or_else(|error| panic!("read: {error}"));
            assert!(
                !bytes
                    .windows(sentinel_bytes.len())
                    .any(|window| window == sentinel_bytes),
                "sentinel secret leaked into {}",
                path.display()
            );
            scanned += 1;
        }
    }
    assert!(
        scanned >= 2,
        "sweep must cover the SQLite journal and accounts.json (scanned {scanned})"
    );
    // accounts.json exists on disk and carries the descriptor, sentinel-free.
    let accounts_path = store_dir.join("accounts.json");
    let accounts_text =
        std::fs::read_to_string(&accounts_path).unwrap_or_else(|error| panic!("{error}"));
    assert!(accounts_text.contains(descriptor.alias.as_str()));
}
