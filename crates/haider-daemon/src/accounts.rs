//! CHARTER — the daemon account seam (R7/R10): staged secrets, the
//! hand-off account actor, credential validation, recoverable
//! Keychain+descriptor commit, and the pending-login startup reconciliation.
//!
//! Laws owned here:
//!
//! - The connection HANDS LOGIN OFF and stays readable: `account.login_api`
//!   atomically claims the staged secret and `try_send`s it to the bounded
//!   account actor, which owns the long validation/commit and answers
//!   through the normal sink later. Nothing here is awaited inline on the
//!   connection task (R7).
//! - Once a login claims a stage, the COMMAND — not the connection — owns
//!   the secret for a bounded pending-command TTL; a retryable validation
//!   keeps it so the same command can retry without retyping; expiry or
//!   daemon restart wipes it and answers `restage_required`.
//! - Secrets live only in zeroizing memory, never in receipts, envelopes,
//!   logs, formatted errors, or descriptor JSON. The physical vault alias is
//!   derived from profile identity + command id, so identical display
//!   aliases in different profiles can never collide in the Keychain (R10).
//! - `accounts.json` and the Keychain cannot share one SQLite transaction:
//!   the pending receipt is the recovery protocol. Commit order is Keychain
//!   first, descriptor add/select + parent-fsynced save, then receipt
//!   finalization; a synchronous descriptor-save failure deletes the
//!   just-written vault alias. `reconcile_login_receipts` closes every
//!   crash boundary on the next start.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use haider_accounts::{AccountStore, JsonFileStore, MemoryVault, StoreLike, Vault};
use haider_core::SqliteStoreHandle;
use haider_core::{LoginClaim, LoginReceiptFailure, LoginReceiptResponse};
use haider_protocol::credential::{AuthMethod, CredentialDescriptor, CredentialStatus};
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::ids::CredentialAlias;
use haider_provider::{
    ANTHROPIC_PROVIDER_NAME, AnthropicProvider, Message, Provider, ProviderErrorKind, TurnRequest,
};
use haider_rpc::{
    ERROR_CODE_INVALID_ARGUMENT, ERROR_CODE_PERMISSION_DENIED, ERROR_CODE_PROVIDER_ERROR,
    ERROR_CODE_RESTAGE_REQUIRED, ERROR_CODE_UNAUTHORIZED, RequestId, ResponseBody, StagePurpose,
    WireFrame,
};
use tokio::sync::mpsc;
use tokio::time::Instant;
use zeroize::Zeroizing;

use crate::session_hub::FrameSink;

/// Staged-secret and pending-login-command lifetime (R7/R10: five minutes).
pub(crate) const SECRET_TTL: Duration = Duration::from_secs(300);

/// Bounded account-actor admission; overflow answers typed `busy`.
const ACTOR_CAPACITY: usize = 8;

// ───────────────────────────── transport class ─────────────────────────────

/// Which transport a negotiated connection arrived over.
///
/// `Control` alone must never make raw-secret staging available to a future
/// remote transport (R7): `vault.stage`/`account.login_api` additionally
/// require [`ConnectionTransport::LocalSameUid`], which the UDS listener
/// grants only after its peer-UID gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionTransport {
    /// Authenticated same-UID local UDS (peer UID verified before any byte).
    LocalSameUid,
    /// Any future remote transport (WebSocket): never secret-staging capable.
    Remote,
}

// ──────────────────────────── validator seam ───────────────────────────────

/// Identity facts a successful validation may report (display only).
#[derive(Debug, Clone)]
pub struct ValidatedIdentity {
    pub identity: String,
}

/// R10 step 8 taxonomy: 401 invalid key; 403 authenticated but not permitted
/// for the selected model/endpoint; everything else is "validation
/// unavailable" and retryable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationFailureKind {
    Unauthorized,
    PermissionDenied,
    Unavailable,
}

/// Typed validation failure; the message must never carry provider body or
/// key text into wire errors.
#[derive(Debug, Clone)]
pub struct ValidationError {
    pub kind: ValidationFailureKind,
    pub message: String,
}

/// Fake-injectable credential validator (tests never touch the network).
#[async_trait::async_trait]
pub trait CredentialValidator: Send + Sync {
    /// Whether this daemon can validate credentials for `provider`.
    fn supports(&self, provider: &str) -> bool;

    /// Proves the secret authenticates for `model` (a minimal, already-
    /// audited provider request), without persisting anything.
    async fn validate(
        &self,
        provider: &str,
        model: &str,
        secret: &[u8],
    ) -> Result<ValidatedIdentity, ValidationError>;
}

/// Production validator: one-token Anthropic Messages request through the
/// audited adapter path (R10: no invented authentication endpoint). Consumes
/// just enough of the stream to prove authentication, then drops it (the
/// stream aborts its producer on drop).
pub struct AnthropicValidator;

#[async_trait::async_trait]
impl CredentialValidator for AnthropicValidator {
    fn supports(&self, provider: &str) -> bool {
        provider == ANTHROPIC_PROVIDER_NAME
    }

    async fn validate(
        &self,
        provider: &str,
        model: &str,
        secret: &[u8],
    ) -> Result<ValidatedIdentity, ValidationError> {
        if provider != ANTHROPIC_PROVIDER_NAME {
            return Err(ValidationError {
                kind: ValidationFailureKind::Unavailable,
                message: format!("no validator for provider {provider}"),
            });
        }
        // Mint a SecretHandle through the vault seam (its constructor is
        // deliberately vault-only); the roundtrip vault is dropped and
        // zeroed immediately after.
        let staging = MemoryVault::default();
        let alias = CredentialAlias::new("login-validation");
        let handle = staging
            .put(&alias, secret)
            .and_then(|()| staging.resolve(&alias))
            .map_err(|error| ValidationError {
                kind: ValidationFailureKind::Unavailable,
                message: format!("validation staging failed: {}", error.message),
            })?;
        let adapter = AnthropicProvider::new(handle, model).map_err(map_provider_error)?;
        let request = TurnRequest {
            messages: vec![Message::user_text("ping")],
            model: model.to_owned(),
            max_tokens: 1,
            system_prompt: None,
            tools: Vec::new(),
            attachments: Vec::new(),
        };
        let mut stream = adapter
            .stream_turn(request)
            .await
            .map_err(map_provider_error)?;
        match stream.recv().await {
            // Any event at all proves the request authenticated; the drop
            // below cancels/drains the stream safely.
            Some(Ok(_)) | None => Ok(ValidatedIdentity {
                identity: "anthropic api key".into(),
            }),
            Some(Err(error)) => Err(map_provider_error(error)),
        }
    }
}

fn map_provider_error(error: haider_provider::ProviderError) -> ValidationError {
    let kind = match error.kind {
        ProviderErrorKind::Authentication => ValidationFailureKind::Unauthorized,
        ProviderErrorKind::PermissionDenied => ValidationFailureKind::PermissionDenied,
        _ => ValidationFailureKind::Unavailable,
    };
    ValidationError {
        kind,
        // Deliberately the adapter's typed kind, not the provider body.
        message: format!("credential validation reported {:?}", error.kind),
    }
}

// ─────────────────────────── daemon dependencies ────────────────────────────

/// Whether this daemon has a working secret vault (R10: the W3c gate is
/// macOS; storing plaintext or silently falling back to an environment
/// variable is rejected).
#[derive(Clone)]
pub enum VaultProvision {
    Available(Arc<dyn Vault>),
    Unsupported,
}

impl VaultProvision {
    /// Platform default: macOS Keychain; every other platform is
    /// `Unsupported` and rejects login with stable `vault_unsupported`.
    pub fn platform_default() -> Self {
        #[cfg(target_os = "macos")]
        {
            Self::Available(Arc::new(haider_accounts::KeychainVault::new()))
        }
        #[cfg(not(target_os = "macos"))]
        {
            Self::Unsupported
        }
    }
}

/// Injectable account machinery (production defaults; tests swap pieces).
#[derive(Clone)]
pub struct AccountsDependencies {
    pub vault: VaultProvision,
    pub validator: Arc<dyn CredentialValidator>,
    /// Descriptor persistence override; `None` uses
    /// `<store_dir>/accounts.json` behind the profile lock.
    pub descriptor_store: Option<Arc<dyn StoreLike>>,
}

impl Default for AccountsDependencies {
    fn default() -> Self {
        Self {
            vault: VaultProvision::platform_default(),
            validator: Arc::new(AnthropicValidator),
            descriptor_store: None,
        }
    }
}

/// The one descriptor-store shape the daemon owns.
enum DescriptorStore {
    Json(JsonFileStore),
    Injected(Arc<dyn StoreLike>),
}

impl StoreLike for DescriptorStore {
    fn load(&self) -> Result<Vec<CredentialDescriptor>, HaiderError> {
        match self {
            Self::Json(store) => store.load(),
            Self::Injected(store) => store.load(),
        }
    }

    fn save(&self, descriptors: &[CredentialDescriptor]) -> Result<(), HaiderError> {
        match self {
            Self::Json(store) => store.save(descriptors),
            Self::Injected(store) => store.save(descriptors),
        }
    }
}

// ─────────────────────────── staged secrets ────────────────────────────────

/// One staged secret in connection-scoped memory (never durable).
struct StagedSecret {
    stage_id: String,
    digest: [u8; 32],
    purpose: StagePurpose,
    secret: Zeroizing<Vec<u8>>,
    staged_at: Instant,
    reference: String,
    expires_at_ms: u64,
}

/// Connection-scoped stage store: wiped on disconnect/close; entries expire
/// after [`SECRET_TTL`]; references are random, single-use, and scoped to
/// this connection and daemon instance.
#[derive(Default)]
pub(crate) struct StagedSecrets {
    entries: Vec<StagedSecret>,
}

/// Typed staging failure mapped by the rpc layer.
pub(crate) enum StageError {
    /// Same `stage_id` with different bytes/purpose.
    Mismatch,
    /// Randomness failure.
    Mint(String),
}

impl StagedSecrets {
    /// Stages (or re-acknowledges) one secret; same id + same bytes returns
    /// the same reference (same-connection retry dedupe).
    pub(crate) fn stage(
        &mut self,
        stage_id: &str,
        purpose: StagePurpose,
        secret_bytes: &[u8],
    ) -> Result<(String, u64), StageError> {
        self.sweep_expired();
        let digest = *blake3::hash(secret_bytes).as_bytes();
        if let Some(existing) = self.entries.iter().find(|entry| entry.stage_id == stage_id) {
            if existing.digest == digest && existing.purpose == purpose {
                return Ok((existing.reference.clone(), existing.expires_at_ms));
            }
            return Err(StageError::Mismatch);
        }
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(|error| StageError::Mint(error.to_string()))?;
        let mut reference = String::with_capacity(42);
        reference.push_str("vaultref-");
        for byte in random {
            use std::fmt::Write as _;
            let _ = write!(&mut reference, "{byte:02x}");
        }
        let expires_at_ms = unix_ms_after(SECRET_TTL);
        self.entries.push(StagedSecret {
            stage_id: stage_id.to_owned(),
            digest,
            purpose,
            secret: Zeroizing::new(secret_bytes.to_vec()),
            staged_at: Instant::now(),
            reference: reference.clone(),
            expires_at_ms,
        });
        Ok((reference, expires_at_ms))
    }

    /// Atomically claims one reference: single-use, purpose-checked by the
    /// caller. `None` covers unknown, already-claimed, and expired alike.
    pub(crate) fn claim(&mut self, reference: &str) -> Option<(StagePurpose, Zeroizing<Vec<u8>>)> {
        self.sweep_expired();
        let index = self
            .entries
            .iter()
            .position(|entry| entry.reference == reference)?;
        let entry = self.entries.swap_remove(index);
        Some((entry.purpose, entry.secret))
    }

    fn sweep_expired(&mut self) {
        self.entries
            .retain(|entry| entry.staged_at.elapsed() < SECRET_TTL);
    }
}

fn unix_ms_after(delta: Duration) -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .saturating_add(delta.as_millis())
        .try_into()
        .unwrap_or(u64::MAX)
}

// ───────────────────────── canonical login identity ─────────────────────────

/// Canonical `request_json` of the durable login command (R7): the semantic
/// provider/resolved-model/alias operation plus the derived physical alias —
/// and deliberately NOT the ephemeral vault reference or any secret.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct LoginIdentity {
    pub provider: String,
    pub resolved_model: String,
    pub display_alias: Option<String>,
    pub physical_alias: String,
}

impl LoginIdentity {
    pub(crate) fn canonical_json(&self) -> Result<String, HaiderError> {
        serde_json::to_string(self).map_err(|error| {
            HaiderError::new(
                ErrorCode::InvalidArgument,
                format!("cannot encode login coordinates: {error}"),
                false,
            )
        })
    }
}

/// Stable physical Keychain/vault alias: profile-namespaced (R10) and
/// command-derived, so identical display aliases in two profiles resolve
/// distinct secrets and a login retry recomputes the same alias.
pub(crate) fn physical_alias(profile_id: &str, provider: &str, command_id: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"haider-login-alias-v1\n");
    hasher.update(profile_id.as_bytes());
    hasher.update(b"\n");
    hasher.update(provider.as_bytes());
    hasher.update(b"\n");
    hasher.update(command_id.as_bytes());
    let digest = hasher.finalize().to_hex();
    format!("{provider}-{}", &digest.as_str()[..32])
}

// ─────────────────────────────── the actor ──────────────────────────────────

/// Shared, read-only descriptor view for `account.list` (updated by the
/// actor after every committed mutation; readers never touch the JSON store,
/// preserving its single-writer assumption).
pub(crate) type AccountsSnapshot = Arc<StdMutex<Vec<CredentialDescriptor>>>;

/// What the hub needs to route account traffic (installed like the worker
/// manager).
#[derive(Clone)]
pub(crate) struct AccountsFacade {
    /// `None` when the vault is unsupported (no actor runs).
    pub login: Option<mpsc::Sender<AccountCommand>>,
    pub snapshot: AccountsSnapshot,
    pub vault_supported: bool,
}

/// Correlated response route back to the requesting connection. Disconnect
/// drops only this route, never the durable command.
pub(crate) struct LoginRoute {
    pub request_id: RequestId,
    pub sink: Arc<dyn FrameSink>,
}

/// One handed-off login command.
pub(crate) struct LoginJob {
    pub command_id: String,
    pub provider: String,
    pub display_alias: Option<String>,
    pub validation_model: Option<String>,
    /// Freshly claimed staged secret; `None` on a retry whose stage is gone
    /// (the actor may still hold the pending command's secret).
    pub secret: Option<Zeroizing<Vec<u8>>>,
    pub route: LoginRoute,
}

/// Account actor mailbox items.
pub(crate) enum AccountCommand {
    Login(Box<LoginJob>),
    Shutdown,
}

/// Owned account actor task (single writer of the descriptor store).
pub(crate) struct AccountActorHandle {
    commands: mpsc::Sender<AccountCommand>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl AccountActorHandle {
    pub(crate) fn commands(&self) -> mpsc::Sender<AccountCommand> {
        self.commands.clone()
    }

    /// Graceful drain: stop accepting, finish the in-flight command, join.
    /// Bounded by the caller's drain deadline (R10).
    pub(crate) async fn shutdown(mut self) {
        let _ = self.commands.send(AccountCommand::Shutdown).await;
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }

    /// Abrupt teardown (crash seam): receipts + reconciliation carry truth.
    pub(crate) fn crash(mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl Drop for AccountActorHandle {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

pub(crate) struct AccountActorConfig {
    pub store: SqliteStoreHandle,
    pub accounts: AccountStore<Box<dyn StoreLike>>,
    pub vault: Arc<dyn Vault>,
    pub validator: Arc<dyn CredentialValidator>,
    pub snapshot: AccountsSnapshot,
    pub profile_id: String,
    pub default_model: String,
}

pub(crate) fn start_account_actor(config: AccountActorConfig) -> AccountActorHandle {
    let (commands, receiver) = mpsc::channel(ACTOR_CAPACITY);
    let task = tokio::spawn(run_account_actor(config, receiver));
    AccountActorHandle {
        commands,
        task: Some(task),
    }
}

struct PendingSecret {
    secret: Zeroizing<Vec<u8>>,
    claimed_at: Instant,
}

async fn run_account_actor(
    config: AccountActorConfig,
    mut receiver: mpsc::Receiver<AccountCommand>,
) {
    let AccountActorConfig {
        store,
        mut accounts,
        vault,
        validator,
        snapshot,
        profile_id,
        default_model,
    } = config;
    // Command-owned secrets surviving a retryable validation, bounded by
    // SECRET_TTL; daemon restart wipes them by construction.
    let mut pending: HashMap<String, PendingSecret> = HashMap::new();
    while let Some(command) = receiver.recv().await {
        match command {
            AccountCommand::Shutdown => break,
            AccountCommand::Login(job) => {
                pending.retain(|_, entry| entry.claimed_at.elapsed() < SECRET_TTL);
                handle_login(
                    &store,
                    &mut accounts,
                    vault.as_ref(),
                    validator.as_ref(),
                    &snapshot,
                    &profile_id,
                    &default_model,
                    &mut pending,
                    *job,
                )
                .await;
            }
        }
    }
}

fn respond(route: &LoginRoute, body: ResponseBody) {
    let _ = route.sink.try_send(WireFrame::Response {
        request_id: route.request_id.clone(),
        body,
    });
}

fn respond_error(route: &LoginRoute, code: &str, message: &str, retryable: bool) {
    respond(
        route,
        ResponseBody::Error {
            code: code.into(),
            message: message.into(),
            retryable,
            data: None,
        },
    );
}

fn refresh_snapshot(snapshot: &AccountsSnapshot, accounts: &AccountStore<Box<dyn StoreLike>>) {
    if let Ok(mut view) = snapshot.lock() {
        *view = accounts.list().to_vec();
    }
}

/// The R10 login flow, executed on the actor. See the module charter for the
/// commit-order and ownership laws this implements.
#[allow(clippy::too_many_arguments)]
async fn handle_login(
    store: &SqliteStoreHandle,
    accounts: &mut AccountStore<Box<dyn StoreLike>>,
    vault: &dyn Vault,
    validator: &dyn CredentialValidator,
    snapshot: &AccountsSnapshot,
    profile_id: &str,
    default_model: &str,
    pending: &mut HashMap<String, PendingSecret>,
    job: LoginJob,
) {
    let LoginJob {
        command_id,
        provider,
        display_alias,
        validation_model,
        secret,
        route,
    } = job;
    if !validator.supports(&provider) {
        respond_error(
            &route,
            ERROR_CODE_INVALID_ARGUMENT,
            &format!("no credential validator for provider {provider}"),
            false,
        );
        return;
    }
    let identity = LoginIdentity {
        provider: provider.clone(),
        resolved_model: validation_model.unwrap_or_else(|| default_model.to_owned()),
        display_alias: display_alias.clone(),
        physical_alias: physical_alias(profile_id, &provider, &command_id),
    };
    let request_json = match identity.canonical_json() {
        Ok(json) => json,
        Err(error) => {
            respond_error(&route, ERROR_CODE_INVALID_ARGUMENT, &error.message, false);
            return;
        }
    };
    let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();

    // Transaction A: replay preflight + pending claim (one store call; the
    // generic receipt path — see Store::login_claim_receipt).
    let claim = store
        .login_claim_receipt(command_id.clone(), request_digest, request_json)
        .await;
    let resume = match claim {
        Ok(LoginClaim::Committed(response)) => {
            // Lost-response retry: wipe any fresh stage and replay the
            // committed result (secret drops zeroized here).
            drop(secret);
            pending.remove(&command_id);
            respond(
                &route,
                ResponseBody::AccountLoginApi {
                    descriptor: response.descriptor,
                },
            );
            return;
        }
        Ok(LoginClaim::Fresh) => false,
        Ok(LoginClaim::ResumePending) => true,
        Err(error) => {
            respond_error(
                &route,
                ERROR_CODE_INVALID_ARGUMENT,
                &error.message,
                error.retryable,
            );
            return;
        }
    };

    let alias = CredentialAlias::new(identity.physical_alias.clone());
    if resume {
        // Crash-boundary reconciliation at command time (R10 step 10):
        // descriptor present -> finalize; vault-only -> resume descriptor
        // commit; neither -> continue with a fresh stage below.
        if accounts.get(&alias).is_some() {
            drop(secret);
            pending.remove(&command_id);
            finalize_and_respond(store, accounts, snapshot, &command_id, &alias, &route).await;
            return;
        }
        if vault.resolve(&alias).is_ok() {
            drop(secret);
            pending.remove(&command_id);
            let descriptor = descriptor_for(&identity, &alias, None);
            if let Err(error) = accounts.add(descriptor) {
                respond_error(
                    &route,
                    ERROR_CODE_PROVIDER_ERROR,
                    &format!("resumed descriptor commit failed: {}", error.message),
                    true,
                );
                return;
            }
            finalize_and_respond(store, accounts, snapshot, &command_id, &alias, &route).await;
            return;
        }
    }

    // Fresh validation needs a secret: the just-claimed stage, or the
    // pending command's retained secret, or an explicit restage.
    let secret = match secret {
        Some(secret) => secret,
        None => match pending.remove(&command_id) {
            Some(entry) if entry.claimed_at.elapsed() < SECRET_TTL => entry.secret,
            _ => {
                respond_error(
                    &route,
                    ERROR_CODE_RESTAGE_REQUIRED,
                    "staged secret is no longer available; stage the key again and retry",
                    true,
                );
                return;
            }
        },
    };

    match validator
        .validate(&provider, &identity.resolved_model, &secret)
        .await
    {
        Ok(validated) => {
            // Keychain first (R10 step 9).
            if let Err(error) = vault.put(&alias, &secret) {
                pending.insert(
                    command_id.clone(),
                    PendingSecret {
                        secret,
                        claimed_at: Instant::now(),
                    },
                );
                respond_error(
                    &route,
                    ERROR_CODE_PROVIDER_ERROR,
                    &format!("vault write failed: {}", error.message),
                    true,
                );
                return;
            }
            drop(secret);
            pending.remove(&command_id);
            let descriptor = descriptor_for(&identity, &alias, Some(validated.identity));
            if let Err(error) = accounts.add(descriptor) {
                // Synchronous descriptor-save failure deletes the
                // just-written vault alias (R10 step 9); the receipt stays
                // pending and a fresh stage retries.
                let _ = vault.delete(&alias);
                respond_error(
                    &route,
                    ERROR_CODE_PROVIDER_ERROR,
                    &format!("descriptor save failed: {}", error.message),
                    true,
                );
                return;
            }
            finalize_and_respond(store, accounts, snapshot, &command_id, &alias, &route).await;
        }
        Err(error) => match error.kind {
            ValidationFailureKind::Unauthorized | ValidationFailureKind::PermissionDenied => {
                // Definitive: wipe immediately, record terminally, nothing
                // else persists (R10 step 8).
                drop(secret);
                pending.remove(&command_id);
                let code = match error.kind {
                    ValidationFailureKind::Unauthorized => ERROR_CODE_UNAUTHORIZED,
                    _ => ERROR_CODE_PERMISSION_DENIED,
                };
                let failure = LoginReceiptFailure {
                    code: code.into(),
                    message: error.message.clone(),
                };
                if let Err(store_error) = store.fail_login_receipt(command_id, failure).await {
                    respond_error(
                        &route,
                        ERROR_CODE_PROVIDER_ERROR,
                        &store_error.message,
                        true,
                    );
                    return;
                }
                respond_error(&route, code, &error.message, false);
            }
            ValidationFailureKind::Unavailable => {
                // Retryable: the COMMAND keeps the secret until TTL so the
                // same command retries without retyping; receipt stays
                // pending.
                pending.insert(
                    command_id,
                    PendingSecret {
                        secret,
                        claimed_at: Instant::now(),
                    },
                );
                respond_error(&route, ERROR_CODE_PROVIDER_ERROR, &error.message, true);
            }
        },
    }
}

fn descriptor_for(
    identity: &LoginIdentity,
    alias: &CredentialAlias,
    validated_identity: Option<String>,
) -> CredentialDescriptor {
    CredentialDescriptor {
        alias: alias.clone(),
        provider: identity.provider.clone(),
        auth_method: AuthMethod::ApiKey,
        identity: identity
            .display_alias
            .clone()
            .or(validated_identity)
            .unwrap_or_else(|| "api-key login".into()),
        status: CredentialStatus::Ok,
        // A committed login becomes the provider's active credential; the
        // store deselects the previous active in the same snapshot.
        active: true,
    }
}

async fn finalize_and_respond(
    store: &SqliteStoreHandle,
    accounts: &mut AccountStore<Box<dyn StoreLike>>,
    snapshot: &AccountsSnapshot,
    command_id: &str,
    alias: &CredentialAlias,
    route: &LoginRoute,
) {
    let Some(descriptor) = accounts.get(alias).cloned() else {
        respond_error(
            route,
            ERROR_CODE_PROVIDER_ERROR,
            "descriptor disappeared before receipt finalization",
            true,
        );
        return;
    };
    refresh_snapshot(snapshot, accounts);
    let response = LoginReceiptResponse {
        descriptor: descriptor.clone(),
    };
    if let Err(error) = store
        .finalize_login_receipt(command_id.to_owned(), response)
        .await
    {
        // The commit is real (vault + descriptor); the receipt stays
        // pending and reconciliation finalizes it on the next start.
        respond_error(route, ERROR_CODE_PROVIDER_ERROR, &error.message, true);
        return;
    }
    respond(route, ResponseBody::AccountLoginApi { descriptor });
}

// ─────────────────── accounts-backed provider factory ──────────────────────

/// Constructs a provider adapter from a vault-resolved credential — the
/// injectable half of the production factory, so the login→next-turn law is
/// testable with a fake provider while the RESOLUTION path (active
/// descriptor → vault secret → adapter) stays production code.
pub trait AccountProviderBuilder: Send + Sync {
    /// The provider names `session.create` may accept under this builder.
    fn providers(&self) -> std::collections::BTreeSet<String>;

    /// Builds the per-turn provider adapter.
    fn build(
        &self,
        provider: &str,
        credential: haider_accounts::SecretHandle,
        model: &str,
        alias: &CredentialAlias,
    ) -> Result<Arc<dyn Provider>, HaiderError>;
}

/// Production builder: Anthropic only.
pub(crate) struct AnthropicAccountBuilder;

impl AccountProviderBuilder for AnthropicAccountBuilder {
    fn providers(&self) -> std::collections::BTreeSet<String> {
        std::collections::BTreeSet::from([ANTHROPIC_PROVIDER_NAME.to_owned()])
    }

    fn build(
        &self,
        provider: &str,
        credential: haider_accounts::SecretHandle,
        model: &str,
        alias: &CredentialAlias,
    ) -> Result<Arc<dyn Provider>, HaiderError> {
        if provider != ANTHROPIC_PROVIDER_NAME {
            return Err(HaiderError::new(
                ErrorCode::InvalidArgument,
                format!("no account-backed adapter for provider {provider}"),
                false,
            ));
        }
        let adapter = AnthropicProvider::new(credential, model)
            .map_err(|error| {
                HaiderError::new(
                    ErrorCode::ProviderError,
                    format!("cannot construct anthropic adapter: {error}"),
                    false,
                )
            })?
            .with_account(alias.clone());
        Ok(Arc::new(adapter))
    }
}

/// The production `ProviderFactory` (R6/R10): resolves the ACTIVE descriptor
/// for the session's provider once per logical turn — after durable
/// acceptance, before any provider work — so a committed login is picked up
/// by the NEXT logical turn with zero worker changes, while an in-flight
/// turn stays pinned to the provider it resolved.
pub(crate) struct AccountsProviderFactory {
    snapshot: AccountsSnapshot,
    vault: VaultProvision,
    builder: Arc<dyn AccountProviderBuilder>,
}

impl AccountsProviderFactory {
    pub(crate) fn new(
        snapshot: AccountsSnapshot,
        vault: VaultProvision,
        builder: Arc<dyn AccountProviderBuilder>,
    ) -> Self {
        Self {
            snapshot,
            vault,
            builder,
        }
    }
}

#[async_trait::async_trait]
impl crate::worker::ProviderFactory for AccountsProviderFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &haider_protocol::session::SessionMetadataV1,
    ) -> Result<crate::worker::ResolvedTurnProvider, HaiderError> {
        let descriptor = self
            .snapshot
            .lock()
            .ok()
            .and_then(|view| {
                view.iter()
                    .find(|descriptor| {
                        descriptor.provider == metadata.provider && descriptor.active
                    })
                    .cloned()
            })
            .ok_or_else(|| {
                HaiderError::new(
                    ErrorCode::CredentialMissing,
                    format!(
                        "no active credential for provider {}; run /login",
                        metadata.provider
                    ),
                    false,
                )
            })?;
        if descriptor.status != CredentialStatus::Ok {
            return Err(HaiderError::new(
                ErrorCode::Unauthorized,
                format!(
                    "active credential {} is not usable ({:?}); run /login again",
                    descriptor.alias, descriptor.status
                ),
                false,
            ));
        }
        let VaultProvision::Available(vault) = &self.vault else {
            return Err(HaiderError::new(
                ErrorCode::CredentialMissing,
                "this platform has no supported secret vault",
                false,
            ));
        };
        let credential = vault.resolve(&descriptor.alias)?;
        let provider = self.builder.build(
            &metadata.provider,
            credential,
            &metadata.model,
            &descriptor.alias,
        )?;
        Ok(crate::worker::ResolvedTurnProvider {
            provider,
            provider_name: metadata.provider.clone(),
            model: metadata.model.clone(),
            account_alias: Some(descriptor.alias.as_str().to_owned()),
        })
    }
}

// ─────────────────────── startup receipt reconciliation ─────────────────────

/// The R10 step-10 `run_inner` startup phase: reconcile pending AND
/// committed login receipts against vault + descriptor truth.
///
/// - committed + descriptor present: nothing to do.
/// - committed + descriptor missing: re-add from the durable response
///   (self-heal; the Keychain may still hold the secret). Never steals the
///   provider's active slot from a later login.
/// - pending + descriptor present: finalize the receipt.
/// - pending + vault-only: resume the descriptor commit, then finalize.
/// - pending + neither: LEAVE PENDING — the same command with a fresh stage
///   completes it later ("neither waits for the same command with a fresh
///   stage"); restart wiped the staged secret by construction.
pub(crate) async fn reconcile_login_receipts(
    store: &SqliteStoreHandle,
    accounts: &mut AccountStore<Box<dyn StoreLike>>,
    vault: &VaultProvision,
) -> Result<(), HaiderError> {
    let rows = store.login_receipts().await?;
    for row in rows {
        let identity: LoginIdentity = match serde_json::from_str(&row.request_json) {
            Ok(identity) => identity,
            Err(error) => {
                return Err(HaiderError::new(
                    ErrorCode::StoreCorrupt,
                    format!(
                        "login receipt {} has undecodable request coordinates: {error}",
                        row.command_id
                    ),
                    false,
                ));
            }
        };
        let alias = CredentialAlias::new(identity.physical_alias.clone());
        match row.state.as_str() {
            "committed" => {
                if accounts.get(&alias).is_some() {
                    continue;
                }
                let response: LoginReceiptResponse = match row
                    .response_json
                    .as_deref()
                    .map(serde_json::from_str)
                    .transpose()
                {
                    Ok(Some(response)) => response,
                    Ok(None) | Err(_) => {
                        return Err(HaiderError::new(
                            ErrorCode::StoreCorrupt,
                            format!(
                                "committed login receipt {} has no decodable response",
                                row.command_id
                            ),
                            false,
                        ));
                    }
                };
                let mut descriptor = response.descriptor;
                // Self-heal without stealing active from a later login.
                descriptor.active = accounts.active_for_provider(&descriptor.provider).is_none();
                accounts.add(descriptor)?;
            }
            "pending" => {
                if accounts.get(&alias).is_some() {
                    finalize_reconciled(store, accounts, &row.command_id, &alias).await?;
                    continue;
                }
                let VaultProvision::Available(vault) = vault else {
                    continue;
                };
                if vault.resolve(&alias).is_ok() {
                    let descriptor = descriptor_for(&identity, &alias, None);
                    accounts.add(descriptor)?;
                    finalize_reconciled(store, accounts, &row.command_id, &alias).await?;
                }
                // Neither: leave pending for a fresh-stage retry.
            }
            _ => {}
        }
    }
    Ok(())
}

async fn finalize_reconciled(
    store: &SqliteStoreHandle,
    accounts: &AccountStore<Box<dyn StoreLike>>,
    command_id: &str,
    alias: &CredentialAlias,
) -> Result<(), HaiderError> {
    let Some(descriptor) = accounts.get(alias).cloned() else {
        return Ok(());
    };
    store
        .finalize_login_receipt(command_id.to_owned(), LoginReceiptResponse { descriptor })
        .await
}

// ───────────────────────────── runtime wiring ───────────────────────────────

/// Everything `run_inner` owns for accounts: built after turn recovery,
/// drained with the workers.
pub(crate) struct AccountsRuntime {
    pub facade: AccountsFacade,
    pub actor: Option<AccountActorHandle>,
    /// The vault provision the production provider factory shares.
    pub vault: VaultProvision,
}

impl AccountsRuntime {
    /// Loads the descriptor store, runs receipt reconciliation, and starts
    /// the account actor (vault-supported platforms only).
    pub(crate) async fn initialize(
        store: &SqliteStoreHandle,
        dependencies: &AccountsDependencies,
        store_dir: &std::path::Path,
        profile_id: &str,
        default_model: &str,
    ) -> Result<Self, HaiderError> {
        let descriptor_store: Box<dyn StoreLike> = match &dependencies.descriptor_store {
            Some(injected) => Box::new(DescriptorStore::Injected(Arc::clone(injected))),
            None => Box::new(DescriptorStore::Json(JsonFileStore::new(store_dir))),
        };
        let mut accounts = AccountStore::new(descriptor_store)?;
        reconcile_login_receipts(store, &mut accounts, &dependencies.vault).await?;
        let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(accounts.list().to_vec()));
        match &dependencies.vault {
            VaultProvision::Available(vault) => {
                let actor = start_account_actor(AccountActorConfig {
                    store: store.clone(),
                    accounts,
                    vault: Arc::clone(vault),
                    validator: Arc::clone(&dependencies.validator),
                    snapshot: Arc::clone(&snapshot),
                    profile_id: profile_id.to_owned(),
                    default_model: default_model.to_owned(),
                });
                Ok(Self {
                    facade: AccountsFacade {
                        login: Some(actor.commands()),
                        snapshot,
                        vault_supported: true,
                    },
                    actor: Some(actor),
                    vault: dependencies.vault.clone(),
                })
            }
            VaultProvision::Unsupported => Ok(Self {
                facade: AccountsFacade {
                    login: None,
                    snapshot,
                    vault_supported: false,
                },
                actor: None,
                vault: VaultProvision::Unsupported,
            }),
        }
    }
}

#[cfg(test)]
#[path = "accounts_tests.rs"]
mod accounts_tests;
