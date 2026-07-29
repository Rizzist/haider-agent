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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use haider_accounts::{
    AccountStore, JsonFileStore, MemoryVault, Resolver, RotationCallback, RotationDecision,
    RotationTrigger, StoreLike, Vault,
};
use haider_core::SqliteStoreHandle;
use haider_core::{
    AccountAddClaim, AccountAddReceiptResponse, LoginClaim, LoginReceiptFailure,
    LoginReceiptResponse,
};
use haider_protocol::credential::{
    AuthMethod, CredentialDescriptor, CredentialStatus, RotationCause, RotationEvent,
};
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::ids::CredentialAlias;
use haider_provider::{
    ANTHROPIC_OAUTH_PROVIDER_NAME, ANTHROPIC_PROVIDER_NAME, AnthropicProvider,
    BUILTIN_PROVIDER_NAMES, Message, OPENAI_COMPATIBLE_PROVIDER_NAME, OPENAI_OAUTH_PROVIDER_NAME,
    OPENAI_PROVIDER_NAME, OpenAiCompatibleProvider, OpenAiProvider, Provider, ProviderErrorKind,
    TurnRequest,
};
use haider_rpc::{
    ERROR_CODE_INVALID_ARGUMENT, ERROR_CODE_PERMISSION_DENIED, ERROR_CODE_PROVIDER_ERROR,
    ERROR_CODE_RESTAGE_REQUIRED, ERROR_CODE_UNAUTHORIZED, RequestId, ResponseBody, StagePurpose,
    WireFrame,
};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use zeroize::Zeroizing;

use crate::oauth::{
    CredentialBroker, OAuthCoordinator, OAuthCoordinatorConfig, OAuthInferenceAuthMode,
    OAuthInferenceHeaderSet, OAuthProviderCatalog, OAuthReadyClaim, sanctioned_inference,
};
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

/// Anthropic-specific validator retained as a narrow injectable/live-smoke
/// seam. Production uses [`ProviderCredentialValidator`].
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
        validate_provider_api_key(provider, model, secret).await
    }
}

/// Production API-key validator for provider-owned endpoints.
///
/// Endpoint-addressed compatible accounts join this dispatch when W5c carries
/// their `base_url` through the account-management command.
pub struct ProviderCredentialValidator;

#[async_trait::async_trait]
impl CredentialValidator for ProviderCredentialValidator {
    fn supports(&self, provider: &str) -> bool {
        matches!(provider, ANTHROPIC_PROVIDER_NAME | OPENAI_PROVIDER_NAME)
    }

    async fn validate(
        &self,
        provider: &str,
        model: &str,
        secret: &[u8],
    ) -> Result<ValidatedIdentity, ValidationError> {
        if !self.supports(provider) {
            return Err(ValidationError {
                kind: ValidationFailureKind::Unavailable,
                message: format!("no validator for provider {provider}"),
            });
        }
        validate_provider_api_key(provider, model, secret).await
    }
}

async fn validate_provider_api_key(
    provider: &str,
    model: &str,
    secret: &[u8],
) -> Result<ValidatedIdentity, ValidationError> {
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
    let adapter: Arc<dyn Provider> = match provider {
        ANTHROPIC_PROVIDER_NAME => {
            Arc::new(AnthropicProvider::new(handle, model).map_err(map_provider_error)?)
        }
        OPENAI_PROVIDER_NAME => {
            Arc::new(OpenAiProvider::new(handle, model).map_err(map_provider_error)?)
        }
        _ => {
            return Err(ValidationError {
                kind: ValidationFailureKind::Unavailable,
                message: format!("no validator for provider {provider}"),
            });
        }
    };
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
            identity: format!("{provider} api key"),
        }),
        Some(Err(error)) => Err(map_provider_error(error)),
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
    /// Release-owned OAuth registrations. Production defaults to the
    /// intentionally empty sanctioned catalog; tests inject only loopback
    /// fake registrations.
    pub oauth_catalog: OAuthProviderCatalog,
    pub oauth_coordinator: OAuthCoordinatorConfig,
}

impl Default for AccountsDependencies {
    fn default() -> Self {
        Self {
            vault: VaultProvision::platform_default(),
            validator: Arc::new(ProviderCredentialValidator),
            descriptor_store: None,
            oauth_catalog: OAuthProviderCatalog::default(),
            oauth_coordinator: OAuthCoordinatorConfig::default(),
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
    pub oauth: Option<OAuthCoordinator>,
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

pub(crate) struct OAuthAddJob {
    pub command_id: String,
    pub provider: String,
    pub display_alias: String,
    pub claim: Option<OAuthReadyClaim>,
    pub route: LoginRoute,
}

/// Account actor mailbox items.
#[derive(Clone)]
pub(crate) struct OAuthRefreshFence {
    pub(crate) generation: u64,
    pub(crate) issuer: String,
    pub(crate) audience: String,
    pub(crate) resource: Option<String>,
    pub(crate) subject_hash: String,
}

pub(crate) enum AccountCommand {
    Login(Box<LoginJob>),
    AddOAuth(Box<OAuthAddJob>),
    BeginOAuthRefresh {
        descriptor: CredentialDescriptor,
        expected: OAuthRefreshFence,
        completed: tokio::sync::oneshot::Sender<Result<bool, HaiderError>>,
    },
    ApplyOAuthRefresh {
        descriptor: CredentialDescriptor,
        expected: OAuthRefreshFence,
        encoded_bundle: Zeroizing<Vec<u8>>,
        completed: tokio::sync::oneshot::Sender<Result<(), RefreshApplyError>>,
    },
    ExpireOAuthRefresh {
        descriptor: CredentialDescriptor,
        expected: OAuthRefreshFence,
        completed: tokio::sync::oneshot::Sender<Result<bool, HaiderError>>,
    },
    ResolveCredential {
        provider: String,
        failure: Option<(CredentialAlias, RotationTrigger)>,
        completed: tokio::sync::oneshot::Sender<Result<ResolvedAccount, HaiderError>>,
    },
    Shutdown,
}

#[derive(Debug)]
pub(crate) struct ResolvedAccount {
    pub descriptor: CredentialDescriptor,
    pub rotation: Option<RotationEvent>,
}

#[derive(Debug)]
pub(crate) enum RefreshApplyError {
    Stale,
    Persist,
}

/// Owned account actor task (single writer of the descriptor store).
pub(crate) struct AccountActorHandle {
    commands: mpsc::Sender<AccountCommand>,
    force_stop: watch::Sender<bool>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl AccountActorHandle {
    pub(crate) fn commands(&self) -> mpsc::Sender<AccountCommand> {
        self.commands.clone()
    }

    /// Graceful drain: stop accepting, finish the in-flight command, join.
    /// Bounded by the caller's drain deadline (R10).
    pub(crate) async fn shutdown(&mut self) {
        let _ = self.commands.send(AccountCommand::Shutdown).await;
        if let Some(task) = self.task.as_mut() {
            let _ = task.await;
        }
        self.task.take();
    }

    /// Forced drain: wake an idle actor, fence a refresh that is inside its
    /// non-cancellable vault write, and join the actor transitively.
    ///
    /// The join is deliberately not replaced with `abort`: a
    /// `spawn_blocking` vault call ignores task abort. Once such a call has
    /// started, the actor must regain control, durably fail-close a rotated
    /// bundle, and drop its zeroizing command bytes before the runtime may
    /// publish `Stopped` or release the profile lock to a successor.
    pub(crate) async fn force_and_join(&mut self) {
        self.force_stop.send_replace(true);
        if let Some(task) = self.task.as_mut() {
            let _ = task.await;
        }
        self.task.take();
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
    let (force_stop, forced) = watch::channel(false);
    let task = tokio::spawn(run_account_actor(config, receiver, forced));
    AccountActorHandle {
        commands,
        force_stop,
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
    mut force_stop: watch::Receiver<bool>,
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
    loop {
        let command = tokio::select! {
            biased;
            changed = force_stop.changed() => {
                if changed.is_err() || *force_stop.borrow() {
                    break;
                }
                continue;
            }
            command = receiver.recv() => {
                let Some(command) = command else {
                    break;
                };
                command
            }
        };
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
            AccountCommand::AddOAuth(job) => {
                handle_oauth_add(
                    &store,
                    &mut accounts,
                    Arc::clone(&vault),
                    &snapshot,
                    &profile_id,
                    *job,
                )
                .await;
            }
            AccountCommand::BeginOAuthRefresh {
                descriptor,
                expected,
                completed,
            } => {
                let result = begin_oauth_refresh(
                    &mut accounts,
                    Arc::clone(&vault),
                    &snapshot,
                    &descriptor,
                    &expected,
                )
                .await;
                let _ = completed.send(result);
            }
            AccountCommand::ExpireOAuthRefresh {
                descriptor,
                expected,
                completed,
            } => {
                let result = expire_oauth_refresh(
                    &mut accounts,
                    Arc::clone(&vault),
                    &snapshot,
                    &descriptor,
                    &expected,
                )
                .await;
                let _ = completed.send(result);
            }
            AccountCommand::ApplyOAuthRefresh {
                descriptor,
                expected,
                encoded_bundle,
                completed,
            } => {
                let result = apply_oauth_refresh(
                    &mut accounts,
                    Arc::clone(&vault),
                    &snapshot,
                    &descriptor,
                    &expected,
                    encoded_bundle,
                    &force_stop,
                )
                .await;
                let _ = completed.send(result);
            }
            AccountCommand::ResolveCredential {
                provider,
                failure,
                completed,
            } => {
                let result =
                    resolve_account(&mut accounts, vault.as_ref(), &snapshot, &provider, failure);
                let _ = completed.send(result);
            }
        }
        if *force_stop.borrow() {
            break;
        }
    }
}

struct AutomaticAlternate<'a> {
    descriptors: &'a [CredentialDescriptor],
    provider: &'a str,
}

impl AutomaticAlternate<'_> {
    fn decide(&self, active: &CredentialAlias, trigger: RotationTrigger) -> RotationDecision {
        let now_ms = unix_ms_after(Duration::ZERO);
        if let Some(alternate) = self.descriptors.iter().find(|descriptor| {
            descriptor.provider == self.provider
                && descriptor.alias != *active
                && match descriptor.status {
                    CredentialStatus::Ok => true,
                    CredentialStatus::Limited { until_ms } => now_ms >= until_ms,
                    CredentialStatus::Expired | CredentialStatus::Revoked => false,
                }
        }) {
            RotationDecision::RotateTo(alternate.alias.clone())
        } else if matches!(trigger, RotationTrigger::RateLimit { .. }) {
            RotationDecision::Wait
        } else {
            RotationDecision::Stop
        }
    }
}

impl RotationCallback for AutomaticAlternate<'_> {
    fn on_limited(&self, alias: &CredentialAlias, until_ms: u64) -> RotationDecision {
        self.decide(alias, RotationTrigger::RateLimit { until_ms })
    }

    fn on_rotation(&self, alias: &CredentialAlias, trigger: RotationTrigger) -> RotationDecision {
        self.decide(alias, trigger)
    }
}

fn resolve_account(
    accounts: &mut AccountStore<Box<dyn StoreLike>>,
    vault: &dyn Vault,
    snapshot: &AccountsSnapshot,
    provider: &str,
    failure: Option<(CredentialAlias, RotationTrigger)>,
) -> Result<ResolvedAccount, HaiderError> {
    let (from, trigger, selected) = if let Some((from, trigger)) = failure {
        let current = accounts.get(&from).cloned().ok_or_else(|| {
            HaiderError::new(
                ErrorCode::CredentialMissing,
                format!("credential alias `{from}` does not exist"),
                false,
            )
        })?;
        if current.provider != provider {
            return Err(HaiderError::new(
                ErrorCode::InvalidArgument,
                format!(
                    "credential alias `{from}` belongs to provider `{}`, not `{provider}`",
                    current.provider
                ),
                false,
            ));
        }
        let status = match trigger {
            RotationTrigger::RateLimit { until_ms } => CredentialStatus::Limited { until_ms },
            RotationTrigger::AuthExpired | RotationTrigger::RefreshFailed => {
                CredentialStatus::Expired
            }
        };
        if current.status != status {
            accounts.set_status(&from, status)?;
            refresh_snapshot(snapshot, accounts);
        }
        let policy = AutomaticAlternate {
            descriptors: accounts.list(),
            provider,
        };
        let resolver = Resolver::new(accounts, vault, &policy);
        let selected = resolver.resolve_alternate_descriptor(provider, &from, trigger)?;
        (from, trigger, selected)
    } else {
        let active = accounts
            .active_for_provider(provider)
            .cloned()
            .ok_or_else(|| {
                HaiderError::new(
                    ErrorCode::CredentialMissing,
                    format!("provider `{provider}` has no active credential"),
                    false,
                )
            })?;
        let trigger = match active.status {
            CredentialStatus::Limited { until_ms } => Some(RotationTrigger::RateLimit { until_ms }),
            CredentialStatus::Expired => Some(RotationTrigger::AuthExpired),
            CredentialStatus::Ok | CredentialStatus::Revoked => None,
        };
        let policy = AutomaticAlternate {
            descriptors: accounts.list(),
            provider,
        };
        let resolver = Resolver::new(accounts, vault, &policy);
        let selected = resolver.resolve_descriptor_for_provider(provider)?;
        let Some(trigger) = trigger.filter(|_| selected.alias != active.alias) else {
            return Ok(ResolvedAccount {
                descriptor: selected,
                rotation: None,
            });
        };
        (active.alias, trigger, selected)
    };

    accounts.select(&selected.alias)?;
    refresh_snapshot(snapshot, accounts);
    Ok(ResolvedAccount {
        rotation: Some(RotationEvent {
            provider: provider.to_owned(),
            from,
            to: selected.alias.clone(),
            cause: trigger.cause(),
        }),
        descriptor: selected,
    })
}

async fn apply_oauth_refresh(
    accounts: &mut AccountStore<Box<dyn StoreLike>>,
    vault: Arc<dyn Vault>,
    snapshot: &AccountsSnapshot,
    descriptor: &CredentialDescriptor,
    expected: &OAuthRefreshFence,
    encoded_bundle: Zeroizing<Vec<u8>>,
    force_stop: &watch::Receiver<bool>,
) -> Result<(), RefreshApplyError> {
    if !accounts
        .get(&descriptor.alias)
        .is_some_and(|current| same_credential_identity(current, descriptor))
    {
        return Err(RefreshApplyError::Stale);
    }
    let alias = descriptor.alias.clone();
    let vault_for_read = Arc::clone(&vault);
    let alias_for_read = alias.clone();
    let current = match tokio::task::spawn_blocking(move || vault_for_read.resolve(&alias_for_read))
        .await
    {
        Ok(Ok(current)) => current,
        Ok(Err(_)) | Err(_) => {
            fail_closed_after_refresh_persist(accounts, Arc::clone(&vault), snapshot, &alias).await;
            return Err(RefreshApplyError::Persist);
        }
    };
    let current = haider_accounts::OAuthTokenBundleV1::decode(current.expose_secret())
        .map_err(|_| RefreshApplyError::Stale)?;
    if current.generation != expected.generation
        || current.issuer != expected.issuer
        || current.audience != expected.audience
        || current.resource != expected.resource
        || current.identity.subject_hash != expected.subject_hash
    {
        return Err(RefreshApplyError::Stale);
    }
    let vault_for_put = Arc::clone(&vault);
    let alias_for_put = alias.clone();
    let persisted =
        tokio::task::spawn_blocking(move || vault_for_put.put(&alias_for_put, &encoded_bundle))
            .await;
    if !matches!(persisted, Ok(Ok(()))) {
        fail_closed_after_refresh_persist(accounts, vault, snapshot, &alias).await;
        return Err(RefreshApplyError::Persist);
    }
    if *force_stop.borrow() {
        // The blocking write began under a daemon that has crossed its drain
        // boundary. It may have published bytes into the physical vault, but
        // it may not restore the descriptor or survive into a successor:
        // join the fail-closed tombstone before the actor owner can finish.
        fail_closed_after_refresh_persist(accounts, vault, snapshot, &alias).await;
        return Err(RefreshApplyError::Persist);
    }
    if accounts
        .get(&alias)
        .is_some_and(|current| matches!(&current.status, CredentialStatus::Expired))
        && !matches!(&descriptor.status, CredentialStatus::Expired)
    {
        if accounts
            .set_status(&alias, descriptor.status.clone())
            .is_err()
        {
            fail_closed_after_refresh_persist(accounts, vault, snapshot, &alias).await;
            return Err(RefreshApplyError::Persist);
        }
        refresh_snapshot(snapshot, accounts);
    }
    Ok(())
}

async fn begin_oauth_refresh(
    accounts: &mut AccountStore<Box<dyn StoreLike>>,
    vault: Arc<dyn Vault>,
    snapshot: &AccountsSnapshot,
    descriptor: &CredentialDescriptor,
    expected: &OAuthRefreshFence,
) -> Result<bool, HaiderError> {
    if !accounts
        .get(&descriptor.alias)
        .is_some_and(|current| same_credential_identity(current, descriptor))
    {
        return Ok(false);
    }
    let vault_for_read = Arc::clone(&vault);
    let alias_for_read = descriptor.alias.clone();
    let current =
        match tokio::task::spawn_blocking(move || vault_for_read.resolve(&alias_for_read)).await {
            Ok(Ok(current)) => current,
            Ok(Err(_)) | Err(_) => return Ok(false),
        };
    let Ok(current) = haider_accounts::OAuthTokenBundleV1::decode(current.expose_secret()) else {
        return Ok(false);
    };
    if current.generation != expected.generation
        || current.provider_id != descriptor.provider
        || current.issuer != expected.issuer
        || current.audience != expected.audience
        || current.resource != expected.resource
        || current.identity.subject_hash != expected.subject_hash
        || current.identity.display_identity != descriptor.identity
    {
        return Ok(false);
    }
    // Durable uncertainty marker before the request can rotate server state.
    // On ordinary storage this retains the original bundle (for audit and
    // issuer-mismatch evidence) while Expired prevents any restart from
    // resolving/retrying it. If descriptor persistence itself fails, the
    // vault tombstone is the fail-closed fallback.
    persist_refresh_expired_or_tombstone(accounts, vault, snapshot, &descriptor.alias).await?;
    Ok(true)
}

async fn expire_oauth_refresh(
    accounts: &mut AccountStore<Box<dyn StoreLike>>,
    vault: Arc<dyn Vault>,
    snapshot: &AccountsSnapshot,
    descriptor: &CredentialDescriptor,
    expected: &OAuthRefreshFence,
) -> Result<bool, HaiderError> {
    if !accounts
        .get(&descriptor.alias)
        .is_some_and(|current| same_credential_identity(current, descriptor))
    {
        return Ok(false);
    }
    let vault_for_read = Arc::clone(&vault);
    let alias_for_read = descriptor.alias.clone();
    let current =
        match tokio::task::spawn_blocking(move || vault_for_read.resolve(&alias_for_read)).await {
            Ok(Ok(current)) => current,
            Ok(Err(_)) | Err(_) => return Ok(false),
        };
    let Ok(current) = haider_accounts::OAuthTokenBundleV1::decode(current.expose_secret()) else {
        return Ok(false);
    };
    if current.generation != expected.generation
        || current.provider_id != descriptor.provider
        || current.issuer != expected.issuer
        || current.audience != expected.audience
        || current.resource != expected.resource
        || current.identity.subject_hash != expected.subject_hash
        || current.identity.display_identity != descriptor.identity
    {
        return Ok(false);
    }
    persist_refresh_expired_or_tombstone(accounts, vault, snapshot, &descriptor.alias).await?;
    Ok(true)
}

fn same_credential_identity(
    current: &CredentialDescriptor,
    expected: &CredentialDescriptor,
) -> bool {
    current.alias == expected.alias
        && current.provider == expected.provider
        && current.base_url == expected.base_url
        && current.auth_method == expected.auth_method
        && current.identity == expected.identity
}

async fn fail_closed_after_refresh_persist(
    accounts: &mut AccountStore<Box<dyn StoreLike>>,
    vault: Arc<dyn Vault>,
    snapshot: &AccountsSnapshot,
    alias: &CredentialAlias,
) {
    // Once the server has rotated the refresh token, a failed local write
    // makes the old vault value unsafe to retry. Persist both fail-closed
    // barriers independently: Expired keeps normal account resolution out,
    // while deletion makes even a stale descriptor unable to recover the
    // server-invalidated token after restart.
    let status_persisted = accounts
        .set_status(alias, CredentialStatus::Expired)
        .is_ok();
    if status_persisted {
        refresh_snapshot(snapshot, accounts);
    } else {
        mark_snapshot_expired(snapshot, alias);
    }
    let vault_for_delete = vault;
    let alias_for_delete = alias.clone();
    let tombstoned =
        tokio::task::spawn_blocking(move || vault_for_delete.delete(&alias_for_delete)).await;
    if !matches!(tombstoned, Ok(Ok(()))) && !status_persisted {
        // Both durable barriers failed. The actor's public snapshot remains
        // closed for this process; the command still reports Persist to its
        // caller, and a production test pins the recoverable single-failure
        // boundary (descriptor failure + durable vault tombstone).
        mark_snapshot_expired(snapshot, alias);
    }
}

async fn persist_refresh_expired_or_tombstone(
    accounts: &mut AccountStore<Box<dyn StoreLike>>,
    vault: Arc<dyn Vault>,
    snapshot: &AccountsSnapshot,
    alias: &CredentialAlias,
) -> Result<(), HaiderError> {
    match accounts.set_status(alias, CredentialStatus::Expired) {
        Ok(()) => {
            refresh_snapshot(snapshot, accounts);
            Ok(())
        }
        Err(status_error) => {
            // A rotating server may already have invalidated the stored
            // refresh token. If the descriptor snapshot cannot durably record
            // Expired, deleting the vault value is the durable fail-closed
            // tombstone: restart can no longer resolve and retry the dead
            // token.
            let vault_for_delete = vault;
            let alias_for_delete = alias.clone();
            match tokio::task::spawn_blocking(move || vault_for_delete.delete(&alias_for_delete))
                .await
            {
                Ok(Ok(())) => {
                    mark_snapshot_expired(snapshot, alias);
                    Ok(())
                }
                Ok(Err(delete_error)) => Err(HaiderError::new(
                    ErrorCode::ProviderError,
                    format!(
                        "OAuth refresh expiration and vault tombstone both failed: {}; {}",
                        status_error.message, delete_error.message
                    ),
                    false,
                )),
                Err(_) => Err(HaiderError::new(
                    ErrorCode::ProviderError,
                    "OAuth refresh expiration failed and vault tombstone worker was lost",
                    false,
                )),
            }
        }
    }
}

fn mark_snapshot_expired(snapshot: &AccountsSnapshot, alias: &CredentialAlias) {
    if let Ok(mut descriptors) = snapshot.lock()
        && let Some(descriptor) = descriptors
            .iter_mut()
            .find(|descriptor| descriptor.alias == *alias)
    {
        descriptor.status = CredentialStatus::Expired;
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

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct OAuthAddIdentity {
    provider: String,
    display_alias: String,
    physical_alias: String,
    auth_method: String,
}

impl OAuthAddIdentity {
    fn canonical_json(&self) -> Result<String, HaiderError> {
        serde_json::to_string(self).map_err(|error| {
            HaiderError::new(
                ErrorCode::InvalidArgument,
                format!("cannot encode OAuth account coordinates: {error}"),
                false,
            )
        })
    }
}

/// Durable OAuth add: receipt claim -> vault bundle -> descriptor -> receipt.
///
/// The claimed bundle is command-owned before this function is entered.
/// Neither it nor the ready reference is ever serialized into SQLite.
async fn handle_oauth_add(
    store: &SqliteStoreHandle,
    accounts: &mut AccountStore<Box<dyn StoreLike>>,
    vault: Arc<dyn Vault>,
    snapshot: &AccountsSnapshot,
    profile_id: &str,
    job: OAuthAddJob,
) {
    let OAuthAddJob {
        command_id,
        provider,
        display_alias,
        claim,
        route,
    } = job;
    let identity = OAuthAddIdentity {
        provider: provider.clone(),
        display_alias,
        physical_alias: physical_alias(profile_id, &provider, &command_id),
        auth_method: "oauth".into(),
    };
    let request_json = match identity.canonical_json() {
        Ok(json) => json,
        Err(error) => {
            respond_error(&route, ERROR_CODE_INVALID_ARGUMENT, &error.message, false);
            return;
        }
    };
    let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
    let receipt = store
        .account_add_claim_receipt(command_id.clone(), request_digest, request_json)
        .await;
    let resume = match receipt {
        Ok(AccountAddClaim::Committed(response)) => {
            respond(
                &route,
                ResponseBody::AccountAdd {
                    descriptor: response.descriptor,
                },
            );
            return;
        }
        Ok(AccountAddClaim::Fresh) => false,
        Ok(AccountAddClaim::ResumePending) => true,
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
    if resume && accounts.get(&alias).is_some() {
        finalize_oauth_add(store, accounts, snapshot, &command_id, &alias, &route).await;
        return;
    }
    if resume {
        let vault_for_read = Arc::clone(&vault);
        let alias_for_read = alias.clone();
        let stored = tokio::task::spawn_blocking(move || vault_for_read.resolve(&alias_for_read))
            .await
            .ok()
            .and_then(Result::ok);
        if let Some(stored) = stored {
            match haider_accounts::OAuthTokenBundleV1::decode(stored.expose_secret()) {
                Ok(bundle) => {
                    let descriptor = oauth_descriptor(&identity, &alias, &bundle);
                    if let Err(error) = accounts.add(descriptor) {
                        respond_error(
                            &route,
                            ERROR_CODE_PROVIDER_ERROR,
                            &format!("resumed OAuth descriptor commit failed: {}", error.message),
                            true,
                        );
                        return;
                    }
                    finalize_oauth_add(store, accounts, snapshot, &command_id, &alias, &route)
                        .await;
                    return;
                }
                Err(_) => {
                    respond_error(
                        &route,
                        ERROR_CODE_UNAUTHORIZED,
                        "stored OAuth token bundle is invalid; restart sign-in",
                        false,
                    );
                    return;
                }
            }
        }
    }
    let Some(claim) = claim else {
        respond_error(
            &route,
            haider_rpc::ERROR_CODE_OAUTH_FLOW_NOT_FOUND,
            "OAuth ready reference is unavailable; start a fresh browser flow",
            true,
        );
        return;
    };
    if claim.bundle.provider_id != identity.provider {
        respond_error(
            &route,
            ERROR_CODE_INVALID_ARGUMENT,
            "OAuth token provider does not match account.add provider",
            false,
        );
        return;
    }
    let descriptor = oauth_descriptor(&identity, &alias, &claim.bundle);
    let encoded = match claim.bundle.encode() {
        Ok(encoded) => encoded,
        Err(error) => {
            respond_error(&route, ERROR_CODE_UNAUTHORIZED, &error.message, false);
            return;
        }
    };
    let vault_for_put = Arc::clone(&vault);
    let alias_for_put = alias.clone();
    let put =
        tokio::task::spawn_blocking(move || vault_for_put.put(&alias_for_put, &encoded)).await;
    match put {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            respond_error(
                &route,
                ERROR_CODE_PROVIDER_ERROR,
                &format!("OAuth vault write failed: {}", error.message),
                true,
            );
            return;
        }
        Err(_) => {
            respond_error(
                &route,
                ERROR_CODE_PROVIDER_ERROR,
                "OAuth vault worker failed",
                true,
            );
            return;
        }
    }
    if let Err(error) = accounts.add(descriptor) {
        let vault_for_delete = Arc::clone(&vault);
        let alias_for_delete = alias.clone();
        let _ =
            tokio::task::spawn_blocking(move || vault_for_delete.delete(&alias_for_delete)).await;
        respond_error(
            &route,
            ERROR_CODE_PROVIDER_ERROR,
            &format!("OAuth descriptor save failed: {}", error.message),
            true,
        );
        return;
    }
    finalize_oauth_add(store, accounts, snapshot, &command_id, &alias, &route).await;
}

fn oauth_descriptor(
    identity: &OAuthAddIdentity,
    alias: &CredentialAlias,
    bundle: &haider_accounts::OAuthTokenBundleV1,
) -> CredentialDescriptor {
    CredentialDescriptor {
        alias: alias.clone(),
        provider: identity.provider.clone(),
        base_url: None,
        auth_method: AuthMethod::OAuth,
        identity: bundle.identity.display_identity.clone(),
        status: CredentialStatus::Ok,
        active: true,
    }
}

async fn finalize_oauth_add(
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
            "OAuth descriptor disappeared before receipt finalization",
            true,
        );
        return;
    };
    refresh_snapshot(snapshot, accounts);
    if let Err(error) = store
        .finalize_account_add_receipt(
            command_id.to_owned(),
            AccountAddReceiptResponse {
                descriptor: descriptor.clone(),
            },
        )
        .await
    {
        respond_error(route, ERROR_CODE_PROVIDER_ERROR, &error.message, true);
        return;
    }
    respond(route, ResponseBody::AccountAdd { descriptor });
}

fn descriptor_for(
    identity: &LoginIdentity,
    alias: &CredentialAlias,
    validated_identity: Option<String>,
) -> CredentialDescriptor {
    CredentialDescriptor {
        alias: alias.clone(),
        provider: identity.provider.clone(),
        base_url: None,
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

    /// Descriptor-aware construction. Existing injected builders remain
    /// source-compatible; production overrides this to consume `base_url`.
    fn build_descriptor(
        &self,
        descriptor: &CredentialDescriptor,
        credential: haider_accounts::SecretHandle,
        model: &str,
    ) -> Result<Arc<dyn Provider>, HaiderError> {
        self.build(&descriptor.provider, credential, model, &descriptor.alias)
    }
}

/// Production builder for every account-backed adapter shipped in this lane.
pub(crate) struct ProductionAccountBuilder;

impl AccountProviderBuilder for ProductionAccountBuilder {
    fn providers(&self) -> std::collections::BTreeSet<String> {
        BUILTIN_PROVIDER_NAMES
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    fn build(
        &self,
        provider: &str,
        credential: haider_accounts::SecretHandle,
        model: &str,
        alias: &CredentialAlias,
    ) -> Result<Arc<dyn Provider>, HaiderError> {
        build_account_provider(provider, None, AuthMethod::ApiKey, credential, model, alias)
    }

    fn build_descriptor(
        &self,
        descriptor: &CredentialDescriptor,
        credential: haider_accounts::SecretHandle,
        model: &str,
    ) -> Result<Arc<dyn Provider>, HaiderError> {
        build_account_provider(
            &descriptor.provider,
            descriptor.base_url.as_deref(),
            descriptor.auth_method,
            credential,
            model,
            &descriptor.alias,
        )
    }
}

fn build_account_provider(
    provider: &str,
    base_url: Option<&str>,
    auth_method: AuthMethod,
    credential: haider_accounts::SecretHandle,
    model: &str,
    alias: &CredentialAlias,
) -> Result<Arc<dyn Provider>, HaiderError> {
    let adapter: Arc<dyn Provider> = match (provider, auth_method) {
        (ANTHROPIC_PROVIDER_NAME, AuthMethod::ApiKey) => Arc::new(
            AnthropicProvider::new(credential, model)
                .map_err(|error| adapter_construction_error(provider, error))?
                .with_account(alias.clone()),
        ),
        (OPENAI_PROVIDER_NAME, AuthMethod::ApiKey) => Arc::new(
            OpenAiProvider::new(credential, model)
                .map_err(|error| adapter_construction_error(provider, error))?
                .with_account(alias.clone()),
        ),
        (OPENAI_COMPATIBLE_PROVIDER_NAME, AuthMethod::ApiKey) => {
            let base_url = base_url.ok_or_else(|| {
                HaiderError::new(
                    ErrorCode::InvalidArgument,
                    "openai-compatible credential is missing base_url",
                    false,
                )
            })?;
            Arc::new(
                OpenAiCompatibleProvider::new(credential, model, base_url)
                    .map_err(|error| adapter_construction_error(provider, error))?
                    .with_account(alias.clone()),
            )
        }
        (OPENAI_OAUTH_PROVIDER_NAME, AuthMethod::OAuth) => {
            let inference = sanctioned_inference(provider).ok_or_else(|| {
                HaiderError::new(
                    ErrorCode::Unauthorized,
                    "OpenAI subscription OAuth registration is unavailable",
                    false,
                )
            })?;
            if inference.auth_mode != OAuthInferenceAuthMode::Bearer
                || inference.header_set != OAuthInferenceHeaderSet::OpenAiCodexResponsesLite
            {
                return Err(HaiderError::new(
                    ErrorCode::Unauthorized,
                    "OpenAI subscription inference metadata is invalid",
                    false,
                ));
            }
            Arc::new(
                OpenAiProvider::new_subscription(credential, model, inference.base_url)
                    .map_err(|error| adapter_construction_error(provider, error))?
                    .with_account(alias.clone()),
            )
        }
        (ANTHROPIC_OAUTH_PROVIDER_NAME, AuthMethod::OAuth) => {
            let inference = sanctioned_inference(provider).ok_or_else(|| {
                HaiderError::new(
                    ErrorCode::Unauthorized,
                    "Anthropic subscription OAuth registration is unavailable",
                    false,
                )
            })?;
            if inference.auth_mode != OAuthInferenceAuthMode::Bearer
                || inference.header_set != OAuthInferenceHeaderSet::AnthropicOAuthBeta
            {
                return Err(HaiderError::new(
                    ErrorCode::Unauthorized,
                    "Anthropic subscription inference metadata is invalid",
                    false,
                ));
            }
            Arc::new(
                AnthropicProvider::new_subscription(credential, model, inference.base_url)
                    .map_err(|error| adapter_construction_error(provider, error))?
                    .with_account(alias.clone()),
            )
        }
        _ => {
            return Err(HaiderError::new(
                ErrorCode::InvalidArgument,
                format!(
                    "no account-backed adapter for provider {provider} with {auth_method:?} authentication"
                ),
                false,
            ));
        }
    };
    Ok(adapter)
}

fn adapter_construction_error(
    provider: &str,
    error: haider_provider::ProviderError,
) -> HaiderError {
    HaiderError::new(
        ErrorCode::ProviderError,
        format!("cannot construct {provider} adapter: {error}"),
        false,
    )
}

#[derive(Clone)]
pub(crate) struct AccountsProviderFactory {
    snapshot: AccountsSnapshot,
    vault: VaultProvision,
    builder: Arc<dyn AccountProviderBuilder>,
    broker: Option<CredentialBroker>,
}

struct ReadOnlySnapshotStore(Vec<CredentialDescriptor>);

impl StoreLike for ReadOnlySnapshotStore {
    fn load(&self) -> Result<Vec<CredentialDescriptor>, HaiderError> {
        Ok(self.0.clone())
    }

    fn save(&self, _descriptors: &[CredentialDescriptor]) -> Result<(), HaiderError> {
        Err(HaiderError::new(
            ErrorCode::StoreLocked,
            "read-only account snapshot cannot be mutated",
            true,
        ))
    }
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
            broker: None,
        }
    }

    pub(crate) fn with_broker(
        snapshot: AccountsSnapshot,
        vault: VaultProvision,
        builder: Arc<dyn AccountProviderBuilder>,
        broker: CredentialBroker,
    ) -> Self {
        Self {
            snapshot,
            vault,
            builder,
            broker: Some(broker),
        }
    }

    async fn resolve_account(
        &self,
        provider: &str,
        failure: Option<(CredentialAlias, RotationTrigger)>,
    ) -> Result<ResolvedAccount, HaiderError> {
        if let Some(broker) = &self.broker {
            return broker.resolve_account(provider, failure).await;
        }
        if failure.is_some() {
            return Err(HaiderError::new(
                ErrorCode::ProviderError,
                "live account rotation requires the account resolver service",
                false,
            ));
        }
        let descriptors = self
            .snapshot
            .lock()
            .map(|snapshot| snapshot.clone())
            .map_err(|_| {
                HaiderError::new(
                    ErrorCode::ProviderError,
                    "account snapshot is unavailable",
                    true,
                )
            })?;
        let VaultProvision::Available(vault) = &self.vault else {
            return Err(HaiderError::new(
                ErrorCode::CredentialMissing,
                "this platform has no supported secret vault",
                false,
            ));
        };
        let accounts = AccountStore::new(ReadOnlySnapshotStore(descriptors))?;
        let active = accounts
            .active_for_provider(provider)
            .cloned()
            .ok_or_else(|| {
                HaiderError::new(
                    ErrorCode::CredentialMissing,
                    format!("provider `{provider}` has no active credential"),
                    false,
                )
            })?;
        let policy = AutomaticAlternate {
            descriptors: accounts.list(),
            provider,
        };
        let resolver = Resolver::new(&accounts, vault.as_ref(), &policy);
        let descriptor = resolver.resolve_descriptor_for_provider(provider)?;
        let rotation = (descriptor.alias != active.alias).then(|| RotationEvent {
            provider: provider.to_owned(),
            from: active.alias,
            to: descriptor.alias.clone(),
            cause: match active.status {
                CredentialStatus::Limited { .. } => RotationCause::RateLimit,
                CredentialStatus::Expired | CredentialStatus::Revoked | CredentialStatus::Ok => {
                    RotationCause::Error
                }
            },
        });
        Ok(ResolvedAccount {
            descriptor,
            rotation,
        })
    }

    async fn resolve_secret(
        &self,
        descriptor: &CredentialDescriptor,
    ) -> Result<haider_accounts::SecretHandle, HaiderError> {
        if let Some(broker) = &self.broker {
            broker.resolve(descriptor).await
        } else if descriptor.auth_method == AuthMethod::OAuth {
            Err(HaiderError::new(
                ErrorCode::Unauthorized,
                "OAuth credential requires the auth-aware broker",
                false,
            ))
        } else {
            let VaultProvision::Available(vault) = &self.vault else {
                return Err(HaiderError::new(
                    ErrorCode::CredentialMissing,
                    "this platform has no supported secret vault",
                    false,
                ));
            };
            vault.resolve(&descriptor.alias)
        }
    }

    async fn resolve_provider(
        &self,
        metadata: &haider_protocol::session::SessionMetadataV1,
    ) -> Result<(ResolvedAccount, Arc<dyn Provider>), HaiderError> {
        let mut resolved = self.resolve_account(&metadata.provider, None).await?;
        let credential = match self.resolve_secret(&resolved.descriptor).await {
            Ok(credential) => credential,
            Err(error) => {
                let Some(trigger) = rotation_trigger_from_error(&error) else {
                    return Err(error);
                };
                if resolved.rotation.is_some() {
                    return Err(error);
                }
                resolved = self
                    .resolve_account(
                        &metadata.provider,
                        Some((resolved.descriptor.alias.clone(), trigger)),
                    )
                    .await?;
                self.resolve_secret(&resolved.descriptor).await?
            }
        };
        let provider =
            self.builder
                .build_descriptor(&resolved.descriptor, credential, &metadata.model)?;
        Ok((resolved, provider))
    }
}

struct AccountsAttemptResolver {
    factory: AccountsProviderFactory,
    metadata: haider_protocol::session::SessionMetadataV1,
    auth_refresh_attempted: AtomicBool,
}

impl AccountsAttemptResolver {
    fn new(
        factory: AccountsProviderFactory,
        metadata: haider_protocol::session::SessionMetadataV1,
    ) -> Self {
        Self {
            factory,
            metadata,
            auth_refresh_attempted: AtomicBool::new(false),
        }
    }

    fn current_descriptor(&self, alias: &CredentialAlias) -> Option<CredentialDescriptor> {
        self.factory.snapshot.lock().ok().and_then(|snapshot| {
            snapshot
                .iter()
                .find(|descriptor| {
                    descriptor.alias == *alias && descriptor.provider == self.metadata.provider
                })
                .cloned()
        })
    }
}

impl std::fmt::Debug for AccountsAttemptResolver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AccountsAttemptResolver")
            .field("provider", &self.metadata.provider)
            .field("model", &self.metadata.model)
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl haider_core::ProviderAttemptResolver for AccountsAttemptResolver {
    async fn resolve(
        &self,
        current_account: &CredentialAlias,
        error: &haider_provider::ProviderError,
    ) -> Result<haider_core::ProviderAttemptDecision, HaiderError> {
        let trigger = match error.kind {
            ProviderErrorKind::RateLimited => {
                let Some(delay_ms) = error.retry_after_ms else {
                    return Ok(haider_core::ProviderAttemptDecision::Wait);
                };
                RotationTrigger::RateLimit {
                    until_ms: unix_ms_after(Duration::from_millis(delay_ms)),
                }
            }
            ProviderErrorKind::Authentication => {
                let Some(current) = self.current_descriptor(current_account) else {
                    return Ok(haider_core::ProviderAttemptDecision::Stop);
                };
                if current.auth_method == AuthMethod::OAuth
                    && !self.auth_refresh_attempted.swap(true, Ordering::AcqRel)
                {
                    let Some(broker) = &self.factory.broker else {
                        return Ok(haider_core::ProviderAttemptDecision::Stop);
                    };
                    match broker.refresh_after_auth_failure(&current).await {
                        Ok(credential) => {
                            let provider = self.factory.builder.build_descriptor(
                                &current,
                                credential,
                                &self.metadata.model,
                            )?;
                            return Ok(haider_core::ProviderAttemptDecision::Retry {
                                provider,
                                account: current.alias,
                            });
                        }
                        Err(refresh_error) => {
                            let Some(trigger) = rotation_trigger_from_error(&refresh_error) else {
                                return Ok(haider_core::ProviderAttemptDecision::Stop);
                            };
                            trigger
                        }
                    }
                } else {
                    RotationTrigger::AuthExpired
                }
            }
            ProviderErrorKind::PermissionDenied
            | ProviderErrorKind::Overloaded
            | ProviderErrorKind::InvalidRequest
            | ProviderErrorKind::Transport
            | ProviderErrorKind::MalformedFrame
            | ProviderErrorKind::InvalidUtf8
            | ProviderErrorKind::Internal => {
                return Ok(haider_core::ProviderAttemptDecision::Wait);
            }
        };
        let resolved = match self
            .factory
            .resolve_account(
                &self.metadata.provider,
                Some((current_account.clone(), trigger)),
            )
            .await
        {
            Ok(resolved) => resolved,
            // Rotation is an optimization over the provider's own failure, so
            // its bookkeeping must never be MORE fatal than the error that
            // triggered it: anything the resolver reports as retryable — a
            // limited alternate, but equally a transient descriptor-store
            // write — falls back to the ordinary retry/backoff path instead
            // of ending the turn.
            Err(error)
                if error.retryable
                    || error.code == ErrorCode::CredentialLimited
                    || error.code == ErrorCode::Unauthorized =>
            {
                return Ok(if error.retryable {
                    haider_core::ProviderAttemptDecision::Wait
                } else {
                    haider_core::ProviderAttemptDecision::Stop
                });
            }
            Err(error) => return Err(error),
        };
        let Some(rotation) = resolved.rotation else {
            return Ok(haider_core::ProviderAttemptDecision::Stop);
        };
        let credential = self.factory.resolve_secret(&resolved.descriptor).await?;
        let provider = self.factory.builder.build_descriptor(
            &resolved.descriptor,
            credential,
            &self.metadata.model,
        )?;
        Ok(haider_core::ProviderAttemptDecision::Rotate(
            haider_core::ResolvedProviderAttempt {
                provider,
                account: resolved.descriptor.alias,
                rotation,
            },
        ))
    }
}

#[async_trait::async_trait]
impl crate::worker::ProviderFactory for AccountsProviderFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &haider_protocol::session::SessionMetadataV1,
    ) -> Result<crate::worker::ResolvedTurnProvider, HaiderError> {
        let (resolved, provider) = self.resolve_provider(metadata).await?;
        let rotation_budget_consumed = resolved.rotation.is_some();
        Ok(crate::worker::ResolvedTurnProvider {
            provider,
            provider_name: metadata.provider.clone(),
            model: metadata.model.clone(),
            account_alias: Some(resolved.descriptor.alias.as_str().to_owned()),
            initial_rotation: resolved.rotation,
            rotation_budget_consumed,
            attempt_resolver: self.broker.as_ref().map(|_| {
                Arc::new(AccountsAttemptResolver::new(self.clone(), metadata.clone()))
                    as Arc<dyn haider_core::ProviderAttemptResolver>
            }),
        })
    }
}

fn rotation_trigger_from_error(error: &HaiderError) -> Option<RotationTrigger> {
    match error
        .details
        .as_ref()
        .and_then(|details| details.get("rotation_trigger"))
        .and_then(serde_json::Value::as_str)
    {
        Some("auth_expired") => Some(RotationTrigger::AuthExpired),
        Some("refresh_failed") => Some(RotationTrigger::RefreshFailed),
        _ => None,
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

pub(crate) async fn reconcile_oauth_add_receipts(
    store: &SqliteStoreHandle,
    accounts: &mut AccountStore<Box<dyn StoreLike>>,
    vault: &VaultProvision,
) -> Result<(), HaiderError> {
    for row in store.account_add_receipts().await? {
        let identity: OAuthAddIdentity =
            serde_json::from_str(&row.request_json).map_err(|error| {
                HaiderError::new(
                    ErrorCode::StoreCorrupt,
                    format!(
                        "OAuth account receipt {} has undecodable coordinates: {error}",
                        row.command_id
                    ),
                    false,
                )
            })?;
        let alias = CredentialAlias::new(identity.physical_alias.clone());
        match row.state.as_str() {
            "committed" if accounts.get(&alias).is_none() => {
                let response: AccountAddReceiptResponse = row
                    .response_json
                    .as_deref()
                    .and_then(|json| serde_json::from_str(json).ok())
                    .ok_or_else(|| {
                        HaiderError::new(
                            ErrorCode::StoreCorrupt,
                            format!(
                                "committed OAuth account receipt {} has no response",
                                row.command_id
                            ),
                            false,
                        )
                    })?;
                let mut descriptor = response.descriptor;
                descriptor.active = accounts.active_for_provider(&descriptor.provider).is_none();
                accounts.add(descriptor)?;
            }
            "pending" if accounts.get(&alias).is_some() => {
                finalize_oauth_reconciled(store, accounts, &row.command_id, &alias).await?;
            }
            "pending" => {
                let VaultProvision::Available(vault) = vault else {
                    continue;
                };
                let Ok(stored) = vault.resolve(&alias) else {
                    // Browser state and ready refs die on restart. The pending
                    // semantic command may resume only with a fresh flow.
                    continue;
                };
                let bundle = haider_accounts::OAuthTokenBundleV1::decode(stored.expose_secret())?;
                if bundle.provider_id != identity.provider {
                    return Err(HaiderError::new(
                        ErrorCode::StoreCorrupt,
                        format!(
                            "OAuth account receipt {} provider does not match vault bundle",
                            row.command_id
                        ),
                        false,
                    ));
                }
                accounts.add(oauth_descriptor(&identity, &alias, &bundle))?;
                finalize_oauth_reconciled(store, accounts, &row.command_id, &alias).await?;
            }
            _ => {}
        }
    }
    Ok(())
}

async fn finalize_oauth_reconciled(
    store: &SqliteStoreHandle,
    accounts: &AccountStore<Box<dyn StoreLike>>,
    command_id: &str,
    alias: &CredentialAlias,
) -> Result<(), HaiderError> {
    let Some(descriptor) = accounts.get(alias).cloned() else {
        return Ok(());
    };
    store
        .finalize_account_add_receipt(
            command_id.to_owned(),
            AccountAddReceiptResponse { descriptor },
        )
        .await
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
    pub broker: Option<CredentialBroker>,
}

impl AccountsRuntime {
    /// Loads the descriptor store, runs receipt reconciliation, and starts
    /// the account actor (vault-supported platforms only).
    pub(crate) async fn initialize(
        store: &SqliteStoreHandle,
        dependencies: &AccountsDependencies,
        store_dir: &std::path::Path,
        profile_id: &str,
        instance_id: &str,
        default_model: &str,
    ) -> Result<Self, HaiderError> {
        let descriptor_store: Box<dyn StoreLike> = match &dependencies.descriptor_store {
            Some(injected) => Box::new(DescriptorStore::Injected(Arc::clone(injected))),
            None => Box::new(DescriptorStore::Json(JsonFileStore::new(store_dir))),
        };
        let mut accounts = AccountStore::new(descriptor_store)?;
        reconcile_login_receipts(store, &mut accounts, &dependencies.vault).await?;
        reconcile_oauth_add_receipts(store, &mut accounts, &dependencies.vault).await?;
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
                let commands = actor.commands();
                let oauth = OAuthCoordinator::new(
                    instance_id.to_owned(),
                    dependencies.oauth_catalog.clone(),
                    dependencies.oauth_coordinator,
                )
                .map_err(crate::oauth::oauth_error)?;
                let broker = CredentialBroker::new(
                    Arc::clone(vault),
                    dependencies.oauth_catalog.clone(),
                    Arc::clone(&snapshot),
                    commands.clone(),
                )?;
                Ok(Self {
                    facade: AccountsFacade {
                        login: Some(commands),
                        oauth: Some(oauth),
                        snapshot,
                        vault_supported: true,
                    },
                    actor: Some(actor),
                    vault: dependencies.vault.clone(),
                    broker: Some(broker),
                })
            }
            VaultProvision::Unsupported => Ok(Self {
                facade: AccountsFacade {
                    login: None,
                    oauth: None,
                    snapshot,
                    vault_supported: false,
                },
                actor: None,
                vault: VaultProvision::Unsupported,
                broker: None,
            }),
        }
    }
}

#[cfg(test)]
#[path = "accounts_tests.rs"]
mod accounts_tests;
