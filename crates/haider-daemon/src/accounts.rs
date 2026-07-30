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
//!   logs, formatted errors, or descriptor JSON. Descriptor and vault calls
//!   use the same normalized public alias; physical vault-slot naming is not
//!   exposed on the wire.
//! - `accounts.json` and the Keychain cannot share one SQLite transaction:
//!   the pending receipt is the recovery protocol. Commit order is Keychain
//!   first, descriptor add/select + parent-fsynced save, then receipt
//!   finalization; a synchronous descriptor-save failure deletes the
//!   just-written vault alias. `reconcile_login_receipts` closes every
//!   crash boundary on the next start.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use haider_accounts::{
    AccountStore, JsonFileStore, MemoryVault, Resolver, RotationCallback, RotationDecision,
    RotationTrigger, SecretHandle, StoreLike, Vault,
};
use haider_core::SqliteStoreHandle;
use haider_core::{
    ACCOUNT_REMOVE_METHOD, ACCOUNT_SET_ACTIVE_METHOD, ACCOUNT_SET_DEFAULT_MODEL_METHOD,
    AccountAddClaim, AccountAddReceiptResponse, LoginClaim, LoginReceiptFailure,
    LoginReceiptResponse, ManagementClaim, PROVIDER_CONFIGURE_METHOD,
};
use haider_protocol::credential::{
    AuthMethod, CredentialDescriptor, CredentialStatus, RotationCause, RotationEvent,
};
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::ids::CredentialAlias;
use haider_provider::{
    ANTHROPIC_OAUTH_PROVIDER_NAME, ANTHROPIC_PROVIDER_NAME, AnthropicProvider,
    BUILTIN_PROVIDER_NAMES, CatalogError, CatalogSource, DiscoveredCatalog, Message,
    OPENAI_COMPATIBLE_PROVIDER_NAME, OPENAI_OAUTH_PROVIDER_NAME, OPENAI_PROVIDER_NAME,
    OpenAiCompatibleProvider, OpenAiProvider, Provider, ProviderErrorKind, TurnRequest,
    discover_models,
};
use haider_rpc::{
    ERROR_CODE_BUSY, ERROR_CODE_CREDENTIAL_MISSING, ERROR_CODE_INVALID_ARGUMENT,
    ERROR_CODE_PERMISSION_DENIED, ERROR_CODE_PROVIDER_ERROR, ERROR_CODE_RESTAGE_REQUIRED,
    ERROR_CODE_REVISION_CONFLICT, ERROR_CODE_UNAUTHORIZED, ErrorData, ProviderSummaryWire,
    RequestId, ResponseBody, StagePurpose, WireFrame,
};
use subtle::ConstantTimeEq as _;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::Instant;
use zeroize::Zeroizing;

use crate::oauth::{
    CredentialBroker, OAuthCoordinator, OAuthCoordinatorConfig, OAuthInferenceAuthMode,
    OAuthInferenceHeaderSet, OAuthProviderCatalog, OAuthReadyClaim, RefreshFenceRegistry,
    load_oauth_import_bundle, oauth_import_source_spec, sanctioned_inference,
};
use crate::provider_registry::{
    CachedProviderModelSource, JsonProviderRegistryStore, ProductionProviderEndpointValidator,
    ProviderConfigureInput, ProviderEndpointValidator, ProviderModelSourceLike, ProviderRegistry,
    ProviderRegistryStoreLike, initial_provider_profiles,
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
    /// Validates custom provider origins on the account actor's owned task.
    pub provider_endpoint_validator: Arc<dyn ProviderEndpointValidator>,
}

impl Default for AccountsDependencies {
    fn default() -> Self {
        Self {
            vault: VaultProvision::platform_default(),
            validator: Arc::new(ProviderCredentialValidator),
            descriptor_store: None,
            oauth_catalog: OAuthProviderCatalog::default(),
            oauth_coordinator: OAuthCoordinatorConfig::default(),
            provider_endpoint_validator: Arc::new(ProductionProviderEndpointValidator),
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
/// provider/resolved-model/global-alias operation (the legacy field name
/// `physical_alias` now stores that logical alias for receipt compatibility)
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
#[cfg(test)]
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

fn canonical_api_alias(provider: &str, command_id: &str) -> String {
    let provider = provider
        .bytes()
        .map(|byte| {
            let byte = byte.to_ascii_lowercase();
            if byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte) {
                char::from(byte)
            } else {
                '-'
            }
        })
        .collect::<String>();
    let provider = provider.trim_matches(['.', '_', '-']);
    let provider = if provider.is_empty() {
        "provider"
    } else {
        provider
    };
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"haider-account-api-alias-v1\n");
    hasher.update(command_id.as_bytes());
    let digest = hasher.finalize().to_hex();
    let prefix_len = provider.len().min(46);
    format!("{}-api-{}", &provider[..prefix_len], &digest.as_str()[..12])
}

fn normalize_account_alias(alias: &str) -> Result<String, HaiderError> {
    let alias = alias.trim().to_ascii_lowercase();
    let bytes = alias.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 64
        || !bytes[0].is_ascii_alphanumeric()
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(byte))
    {
        return Err(HaiderError::new(
            ErrorCode::InvalidArgument,
            "account alias must match [a-z0-9][a-z0-9._-]{0,63}",
            false,
        ));
    }
    Ok(alias)
}

// ─────────────────────────────── the actor ──────────────────────────────────

/// Shared, read-only descriptor view for `account.list` (updated by the
/// actor after every committed mutation; readers never touch the JSON store,
/// preserving its single-writer assumption).
pub(crate) type AccountsSnapshot = Arc<StdMutex<Vec<CredentialDescriptor>>>;

/// One atomically published management read view. RPC reads take this single
/// lock so account/provider data can never be paired with another revision.
#[derive(Clone)]
pub(crate) struct ManagementSnapshot {
    inner: Arc<StdMutex<ManagementView>>,
}

#[derive(Clone)]
pub(crate) struct ManagementView {
    pub revision: u64,
    pub descriptors: Vec<CredentialDescriptor>,
    pub providers: Vec<ProviderSummaryWire>,
}

impl ManagementSnapshot {
    pub(crate) fn new(
        revision: u64,
        descriptors: Vec<CredentialDescriptor>,
        providers: Vec<ProviderSummaryWire>,
    ) -> Self {
        Self {
            inner: Arc::new(StdMutex::new(ManagementView {
                revision,
                descriptors,
                providers,
            })),
        }
    }

    pub(crate) fn read(&self) -> Option<ManagementView> {
        self.inner.lock().ok().map(|view| view.clone())
    }

    fn publish_accounts(&self, revision: u64, descriptors: Vec<CredentialDescriptor>) {
        if let Ok(mut view) = self.inner.lock() {
            view.revision = revision;
            view.descriptors = descriptors;
        }
    }

    fn publish(
        &self,
        revision: u64,
        descriptors: Vec<CredentialDescriptor>,
        providers: Vec<ProviderSummaryWire>,
    ) {
        if let Ok(mut view) = self.inner.lock() {
            *view = ManagementView {
                revision,
                descriptors,
                providers,
            };
        }
    }
}

/// What the hub needs to route account traffic (installed like the worker
/// manager).
#[derive(Clone)]
pub(crate) struct AccountsFacade {
    /// `None` when the vault is unsupported (no actor runs).
    pub login: Option<mpsc::Sender<AccountCommand>>,
    pub oauth: Option<OAuthCoordinator>,
    pub snapshot: AccountsSnapshot,
    pub management: ManagementSnapshot,
    pub vault_supported: bool,
}

/// Correlated response route back to the requesting connection. Disconnect
/// drops only this route, never the durable command.
#[derive(Clone)]
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

pub(crate) struct OAuthImportJob {
    pub command_id: String,
    pub source: String,
    pub route: LoginRoute,
}

pub(crate) struct SetActiveJob {
    pub command_id: String,
    pub alias: String,
    pub route: LoginRoute,
}

pub(crate) struct RemoveAccountJob {
    pub command_id: String,
    pub alias: String,
    pub expected_revision: Option<u64>,
    pub route: LoginRoute,
}

pub(crate) struct SetDefaultModelJob {
    pub command_id: String,
    pub provider: String,
    pub model: String,
    pub expected_revision: u64,
    pub route: LoginRoute,
}

pub(crate) struct ProviderConfigureJob {
    pub command_id: String,
    pub input: ProviderConfigureInput,
    pub expected_revision: u64,
    pub route: LoginRoute,
}

/// Account actor mailbox items.
#[derive(Clone)]
pub(crate) struct OAuthRefreshFence {
    pub(crate) fence_epoch: u64,
    pub(crate) generation: u64,
    pub(crate) issuer: String,
    pub(crate) audience: String,
    pub(crate) resource: Option<String>,
    pub(crate) subject_hash: String,
}

pub(crate) enum AccountCommand {
    Login(Box<LoginJob>),
    AddOAuth(Box<OAuthAddJob>),
    ImportOAuth(Box<OAuthImportJob>),
    SetActive(Box<SetActiveJob>),
    Remove(Box<RemoveAccountJob>),
    SetDefaultModel(Box<SetDefaultModelJob>),
    ConfigureProvider(Box<ProviderConfigureJob>),
    RefreshProviderModels {
        provider: String,
        completed: LoginRoute,
    },
    ProviderModelsRefreshCompleted {
        provider: String,
        cached: Option<haider_core::CachedModels>,
        result: ProviderModelsRefreshResult,
        completed: LoginRoute,
    },
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

pub(crate) enum ProviderModelsRefreshResult {
    Discovery(Result<DiscoveredCatalog, CatalogError>),
    Credential(HaiderError),
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
    pub management: Option<ManagementSnapshot>,
    pub profile_id: String,
    pub default_model: String,
    pub providers: ProviderRegistry<Box<dyn ProviderRegistryStoreLike>>,
    pub provider_endpoint_validator: Arc<dyn ProviderEndpointValidator>,
    pub reserved_aliases: HashSet<String>,
    pub refresh_fences: RefreshFenceRegistry,
}

#[async_trait::async_trait]
trait ProviderModelDiscoverer: Send + Sync {
    async fn discover(
        &self,
        source: CatalogSource,
        access_token: &str,
        etag: Option<&str>,
    ) -> Result<DiscoveredCatalog, CatalogError>;
}

struct ProductionProviderModelDiscoverer;

#[async_trait::async_trait]
impl ProviderModelDiscoverer for ProductionProviderModelDiscoverer {
    async fn discover(
        &self,
        source: CatalogSource,
        access_token: &str,
        etag: Option<&str>,
    ) -> Result<DiscoveredCatalog, CatalogError> {
        discover_models(source, access_token, etag).await
    }
}

pub(crate) fn start_account_actor(config: AccountActorConfig) -> AccountActorHandle {
    let (commands, receiver) = mpsc::channel(ACTOR_CAPACITY);
    let (force_stop, forced) = watch::channel(false);
    spawn_account_actor(
        config,
        commands,
        receiver,
        force_stop,
        forced,
        None,
        Arc::new(ProductionProviderModelDiscoverer),
    )
}

fn start_account_actor_with_broker(
    config: AccountActorConfig,
    build_broker: impl FnOnce(mpsc::Sender<AccountCommand>) -> Result<CredentialBroker, HaiderError>,
) -> Result<(AccountActorHandle, CredentialBroker), HaiderError> {
    start_account_actor_with_services(
        config,
        build_broker,
        Arc::new(ProductionProviderModelDiscoverer),
    )
}

fn start_account_actor_with_services(
    config: AccountActorConfig,
    build_broker: impl FnOnce(mpsc::Sender<AccountCommand>) -> Result<CredentialBroker, HaiderError>,
    model_discoverer: Arc<dyn ProviderModelDiscoverer>,
) -> Result<(AccountActorHandle, CredentialBroker), HaiderError> {
    let (commands, receiver) = mpsc::channel(ACTOR_CAPACITY);
    let (force_stop, forced) = watch::channel(false);
    let broker = build_broker(commands.clone())?;
    let handle = spawn_account_actor(
        config,
        commands,
        receiver,
        force_stop,
        forced,
        Some(broker.clone()),
        model_discoverer,
    );
    Ok((handle, broker))
}

fn spawn_account_actor(
    config: AccountActorConfig,
    commands: mpsc::Sender<AccountCommand>,
    receiver: mpsc::Receiver<AccountCommand>,
    force_stop: watch::Sender<bool>,
    forced: watch::Receiver<bool>,
    broker: Option<CredentialBroker>,
    model_discoverer: Arc<dyn ProviderModelDiscoverer>,
) -> AccountActorHandle {
    let task = tokio::spawn(run_account_actor(
        config,
        commands.clone(),
        receiver,
        forced,
        broker,
        model_discoverer,
    ));
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
    commands: mpsc::Sender<AccountCommand>,
    mut receiver: mpsc::Receiver<AccountCommand>,
    mut force_stop: watch::Receiver<bool>,
    broker: Option<CredentialBroker>,
    model_discoverer: Arc<dyn ProviderModelDiscoverer>,
) {
    let AccountActorConfig {
        store,
        mut accounts,
        vault,
        validator,
        snapshot,
        management,
        profile_id,
        default_model,
        mut providers,
        provider_endpoint_validator,
        mut reserved_aliases,
        refresh_fences,
    } = config;
    // Command-owned secrets surviving a retryable validation, bounded by
    // SECRET_TTL; daemon restart wipes them by construction.
    let mut pending: HashMap<String, PendingSecret> = HashMap::new();
    let mut model_refreshes = JoinSet::new();
    let mut model_refresh_routes = HashMap::new();
    let mut refreshing_providers = HashSet::new();
    let mut draining = false;
    loop {
        if draining && model_refreshes.is_empty() && refreshing_providers.is_empty() {
            break;
        }
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
            completed = model_refreshes.join_next_with_id(), if !model_refreshes.is_empty() => {
                if let Some(completed) = completed {
                    match completed {
                        Ok((task_id, ())) => {
                            model_refresh_routes.remove(&task_id);
                        }
                        Err(error) => {
                            let task_id = error.id();
                            tracing::warn!(%task_id, "provider model refresh worker was lost");
                            if let Some((provider, route)) =
                                model_refresh_routes.remove(&task_id)
                            {
                                refreshing_providers.remove(&provider);
                                respond_error(
                                    &route,
                                    ERROR_CODE_PROVIDER_ERROR,
                                    "provider model refresh worker failed",
                                    true,
                                );
                            }
                        }
                    }
                }
                continue;
            }
        };
        match command {
            AccountCommand::Shutdown => {
                draining = true;
            }
            AccountCommand::Login(job) => {
                pending.retain(|_, entry| entry.claimed_at.elapsed() < SECRET_TTL);
                handle_login(
                    &store,
                    &mut accounts,
                    vault.as_ref(),
                    validator.as_ref(),
                    &snapshot,
                    management.as_ref(),
                    &profile_id,
                    &default_model,
                    &mut pending,
                    &reserved_aliases,
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
                    management.as_ref(),
                    &profile_id,
                    &reserved_aliases,
                    *job,
                )
                .await;
            }
            AccountCommand::ImportOAuth(job) => {
                handle_oauth_import(
                    &store,
                    &mut accounts,
                    Arc::clone(&vault),
                    &snapshot,
                    management.as_ref(),
                    &reserved_aliases,
                    &refresh_fences,
                    *job,
                )
                .await;
            }
            AccountCommand::SetActive(job) => {
                handle_set_active(
                    &store,
                    &mut accounts,
                    &snapshot,
                    management.as_ref(),
                    &providers,
                    *job,
                )
                .await;
            }
            AccountCommand::Remove(job) => {
                handle_remove_account(
                    &store,
                    &mut accounts,
                    Arc::clone(&vault),
                    &snapshot,
                    management.as_ref(),
                    &providers,
                    &mut reserved_aliases,
                    &refresh_fences,
                    *job,
                )
                .await;
            }
            AccountCommand::SetDefaultModel(job) => {
                handle_set_default_model(
                    &store,
                    &accounts,
                    management.as_ref(),
                    &mut providers,
                    *job,
                )
                .await;
            }
            AccountCommand::ConfigureProvider(job) => {
                handle_provider_configure(
                    &store,
                    &accounts,
                    management.as_ref(),
                    &mut providers,
                    Arc::clone(&provider_endpoint_validator),
                    *job,
                )
                .await;
            }
            AccountCommand::RefreshProviderModels {
                provider,
                completed,
            } => {
                if draining {
                    respond_error(
                        &completed,
                        ERROR_CODE_BUSY,
                        "account actor is shutting down",
                        true,
                    );
                    continue;
                }
                begin_provider_models_refresh(
                    &store,
                    &accounts,
                    broker.as_ref(),
                    &model_discoverer,
                    &commands,
                    &mut model_refreshes,
                    &mut model_refresh_routes,
                    &mut refreshing_providers,
                    provider,
                    completed,
                )
                .await;
            }
            AccountCommand::ProviderModelsRefreshCompleted {
                provider,
                cached,
                result,
                completed,
            } => {
                refreshing_providers.remove(&provider);
                finish_provider_models_refresh(
                    &store,
                    &accounts,
                    management.as_ref(),
                    &providers,
                    provider,
                    cached,
                    result,
                    &completed,
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
                    management.as_ref(),
                    &store,
                    &descriptor,
                    &expected,
                    &refresh_fences,
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
                    management.as_ref(),
                    &store,
                    &descriptor,
                    &expected,
                    &refresh_fences,
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
                    management.as_ref(),
                    &store,
                    &descriptor,
                    &expected,
                    encoded_bundle,
                    &refresh_fences,
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
                let result = resolve_account(
                    &store,
                    &mut accounts,
                    vault.as_ref(),
                    &snapshot,
                    management.as_ref(),
                    &provider,
                    failure,
                )
                .await;
                let _ = completed.send(result);
            }
        }
        if *force_stop.borrow() {
            break;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn begin_provider_models_refresh(
    store: &SqliteStoreHandle,
    accounts: &AccountStore<Box<dyn StoreLike>>,
    broker: Option<&CredentialBroker>,
    model_discoverer: &Arc<dyn ProviderModelDiscoverer>,
    commands: &mpsc::Sender<AccountCommand>,
    refresh_tasks: &mut JoinSet<()>,
    refresh_routes: &mut HashMap<tokio::task::Id, (String, LoginRoute)>,
    refreshing_providers: &mut HashSet<String>,
    provider: String,
    completed: LoginRoute,
) {
    let Some(source) = catalog_source(&provider) else {
        respond_provider_models_unavailable(
            &completed,
            &provider,
            "provider does not expose a subscription model catalog",
        );
        return;
    };
    if refreshing_providers.contains(&provider) {
        respond_error(
            &completed,
            ERROR_CODE_BUSY,
            "a model refresh is already running for this provider",
            true,
        );
        return;
    }
    let Some(descriptor) = accounts.active_for_provider(&provider).cloned() else {
        respond_error(
            &completed,
            ERROR_CODE_CREDENTIAL_MISSING,
            "provider has no active credential",
            false,
        );
        return;
    };
    if descriptor.auth_method != AuthMethod::OAuth {
        respond_provider_models_unavailable(
            &completed,
            &provider,
            "provider model discovery requires an active OAuth credential",
        );
        return;
    }
    let Some(broker) = broker.cloned() else {
        respond_error(
            &completed,
            ERROR_CODE_CREDENTIAL_MISSING,
            "OAuth credential broker is unavailable",
            true,
        );
        return;
    };
    let cached = match store.provider_models(provider.clone()).await {
        Ok(cached) => cached,
        Err(error) => {
            respond_error(
                &completed,
                ERROR_CODE_PROVIDER_ERROR,
                &error.message,
                error.retryable,
            );
            return;
        }
    };
    let etag = cached.as_ref().and_then(|cached| cached.etag.clone());
    refreshing_providers.insert(provider.clone());
    let commands = commands.clone();
    let model_discoverer = Arc::clone(model_discoverer);
    let task_provider = provider.clone();
    let task_completed = completed.clone();
    let refresh_task = refresh_tasks.spawn(async move {
        let result = match broker.resolve(&descriptor).await {
            Ok(access_token) => {
                let access_token =
                    std::str::from_utf8(access_token.expose_secret()).map_err(|_| {
                        HaiderError::new(
                            ErrorCode::CredentialMissing,
                            "OAuth access token is not valid UTF-8",
                            false,
                        )
                    });
                match access_token {
                    Ok(access_token) => ProviderModelsRefreshResult::Discovery(
                        model_discoverer
                            .discover(source, access_token, etag.as_deref())
                            .await,
                    ),
                    Err(error) => ProviderModelsRefreshResult::Credential(error),
                }
            }
            Err(error) => ProviderModelsRefreshResult::Credential(error),
        };
        let _ = commands
            .send(AccountCommand::ProviderModelsRefreshCompleted {
                provider: task_provider,
                cached,
                result,
                completed: task_completed,
            })
            .await;
    });
    refresh_routes.insert(refresh_task.id(), (provider, completed));
}

#[allow(clippy::too_many_arguments)]
async fn finish_provider_models_refresh(
    store: &SqliteStoreHandle,
    accounts: &AccountStore<Box<dyn StoreLike>>,
    management: Option<&ManagementSnapshot>,
    providers: &ProviderRegistry<Box<dyn ProviderRegistryStoreLike>>,
    provider: String,
    cached: Option<haider_core::CachedModels>,
    result: ProviderModelsRefreshResult,
    completed: &LoginRoute,
) {
    match result {
        ProviderModelsRefreshResult::Discovery(Ok(catalog)) => {
            let models_json = match serde_json::to_string(&catalog.models) {
                Ok(models_json) => models_json,
                Err(error) => {
                    respond_error(
                        completed,
                        ERROR_CODE_PROVIDER_ERROR,
                        &format!("could not encode provider model catalog: {error}"),
                        false,
                    );
                    return;
                }
            };
            let revision = match store
                .put_provider_models_and_advance_management_revision(
                    provider.clone(),
                    models_json,
                    catalog.etag,
                    unix_ms_after(Duration::ZERO),
                )
                .await
            {
                Ok(revision) => revision,
                Err(error) => {
                    respond_error(
                        completed,
                        ERROR_CODE_PROVIDER_ERROR,
                        &error.message,
                        error.retryable,
                    );
                    return;
                }
            };
            providers.replace_models(provider.clone(), catalog.models);
            let summaries = providers.summaries();
            let Some(summary) = summaries
                .iter()
                .find(|summary| summary.provider == provider)
                .cloned()
            else {
                respond_provider_models_unavailable(
                    completed,
                    &provider,
                    "provider is not registered",
                );
                return;
            };
            if let Some(management) = management {
                management.publish(revision, accounts.list().to_vec(), summaries);
            }
            respond(
                completed,
                ResponseBody::ProviderModelsRefresh {
                    provider: summary,
                    revision,
                },
            );
        }
        ProviderModelsRefreshResult::Discovery(Err(CatalogError::NotModified)) => {
            let Some(cached) = cached else {
                respond_error(
                    completed,
                    ERROR_CODE_PROVIDER_ERROR,
                    "provider returned not-modified without a cached catalog",
                    true,
                );
                return;
            };
            if let Err(error) = store
                .put_provider_models(
                    provider.clone(),
                    cached.models_json,
                    cached.etag,
                    unix_ms_after(Duration::ZERO),
                )
                .await
            {
                respond_error(
                    completed,
                    ERROR_CODE_PROVIDER_ERROR,
                    &error.message,
                    error.retryable,
                );
                return;
            }
            let revision = match store.management_revision().await {
                Ok(revision) => revision,
                Err(error) => {
                    respond_error(
                        completed,
                        ERROR_CODE_PROVIDER_ERROR,
                        &error.message,
                        error.retryable,
                    );
                    return;
                }
            };
            let Some(summary) = providers.summary(&provider) else {
                respond_provider_models_unavailable(
                    completed,
                    &provider,
                    "provider is not registered",
                );
                return;
            };
            respond(
                completed,
                ResponseBody::ProviderModelsRefresh {
                    provider: summary,
                    revision,
                },
            );
        }
        ProviderModelsRefreshResult::Discovery(Err(CatalogError::Unavailable { reason })) => {
            respond_provider_models_unavailable(completed, &provider, &reason);
        }
        ProviderModelsRefreshResult::Discovery(Err(CatalogError::Transport { reason })) => {
            respond_error(completed, ERROR_CODE_PROVIDER_ERROR, &reason, true);
        }
        ProviderModelsRefreshResult::Credential(error) => {
            respond_error(
                completed,
                ERROR_CODE_PROVIDER_ERROR,
                &error.message,
                error.retryable,
            );
        }
    }
}

fn catalog_source(provider: &str) -> Option<CatalogSource> {
    match provider {
        OPENAI_OAUTH_PROVIDER_NAME => Some(CatalogSource::OpenAiSubscription),
        ANTHROPIC_OAUTH_PROVIDER_NAME => Some(CatalogSource::AnthropicSubscription),
        _ => None,
    }
}

fn respond_provider_models_unavailable(route: &LoginRoute, provider: &str, reason: &str) {
    respond(
        route,
        ResponseBody::Error {
            code: ERROR_CODE_PROVIDER_ERROR.into(),
            message: reason.to_owned(),
            retryable: false,
            data: Some(ErrorData::ProviderModelsUnavailable {
                provider: provider.to_owned(),
                reason: reason.to_owned(),
            }),
        },
    );
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

async fn resolve_account(
    store: &SqliteStoreHandle,
    accounts: &mut AccountStore<Box<dyn StoreLike>>,
    vault: &dyn Vault,
    snapshot: &AccountsSnapshot,
    management: Option<&ManagementSnapshot>,
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
        let status_changed = current.status != status;
        if status_changed {
            accounts.set_status(&from, status)?;
        }
        let policy = AutomaticAlternate {
            descriptors: accounts.list(),
            provider,
        };
        let resolver = Resolver::new(accounts, vault, &policy);
        let selected = match resolver.resolve_alternate_descriptor(provider, &from, trigger) {
            Ok(selected) => selected,
            Err(error) => {
                if status_changed {
                    refresh_resolver_snapshot(snapshot, accounts);
                    publish_next_management_revision(store, snapshot, management, accounts).await?;
                }
                return Err(error);
            }
        };
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
    refresh_resolver_snapshot(snapshot, accounts);
    publish_next_management_revision(store, snapshot, management, accounts).await?;
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

#[allow(clippy::too_many_arguments)]
async fn apply_oauth_refresh(
    accounts: &mut AccountStore<Box<dyn StoreLike>>,
    vault: Arc<dyn Vault>,
    snapshot: &AccountsSnapshot,
    management: Option<&ManagementSnapshot>,
    store: &SqliteStoreHandle,
    descriptor: &CredentialDescriptor,
    expected: &OAuthRefreshFence,
    encoded_bundle: Zeroizing<Vec<u8>>,
    refresh_fences: &RefreshFenceRegistry,
    force_stop: &watch::Receiver<bool>,
) -> Result<(), RefreshApplyError> {
    if refresh_fences.current(&descriptor.alias) != expected.fence_epoch {
        return Err(RefreshApplyError::Stale);
    }
    if !accounts
        .get(&descriptor.alias)
        .is_some_and(|current| same_credential_identity(current, descriptor))
    {
        return Err(RefreshApplyError::Stale);
    }
    let alias = descriptor.alias.clone();
    let vault_for_read = Arc::clone(&vault);
    let alias_for_read = alias.clone();
    let current =
        match tokio::task::spawn_blocking(move || vault_for_read.resolve(&alias_for_read)).await {
            Ok(Ok(current)) => current,
            Ok(Err(_)) | Err(_) => {
                fail_closed_after_refresh_persist(
                    store,
                    accounts,
                    Arc::clone(&vault),
                    snapshot,
                    management,
                    &alias,
                )
                .await;
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
        fail_closed_after_refresh_persist(store, accounts, vault, snapshot, management, &alias)
            .await;
        return Err(RefreshApplyError::Persist);
    }
    if *force_stop.borrow() {
        // The blocking write began under a daemon that has crossed its drain
        // boundary. It may have published bytes into the physical vault, but
        // it may not restore the descriptor or survive into a successor:
        // join the fail-closed tombstone before the actor owner can finish.
        fail_closed_after_refresh_persist(store, accounts, vault, snapshot, management, &alias)
            .await;
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
            fail_closed_after_refresh_persist(store, accounts, vault, snapshot, management, &alias)
                .await;
            return Err(RefreshApplyError::Persist);
        }
        refresh_resolver_snapshot(snapshot, accounts);
        if publish_next_management_revision(store, snapshot, management, accounts)
            .await
            .is_err()
        {
            return Err(RefreshApplyError::Persist);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn begin_oauth_refresh(
    accounts: &mut AccountStore<Box<dyn StoreLike>>,
    vault: Arc<dyn Vault>,
    snapshot: &AccountsSnapshot,
    management: Option<&ManagementSnapshot>,
    store: &SqliteStoreHandle,
    descriptor: &CredentialDescriptor,
    expected: &OAuthRefreshFence,
    refresh_fences: &RefreshFenceRegistry,
) -> Result<bool, HaiderError> {
    if refresh_fences.current(&descriptor.alias) != expected.fence_epoch {
        return Ok(false);
    }
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
    persist_refresh_expired_or_tombstone(
        store,
        accounts,
        vault,
        snapshot,
        management,
        &descriptor.alias,
    )
    .await?;
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
async fn expire_oauth_refresh(
    accounts: &mut AccountStore<Box<dyn StoreLike>>,
    vault: Arc<dyn Vault>,
    snapshot: &AccountsSnapshot,
    management: Option<&ManagementSnapshot>,
    store: &SqliteStoreHandle,
    descriptor: &CredentialDescriptor,
    expected: &OAuthRefreshFence,
    refresh_fences: &RefreshFenceRegistry,
) -> Result<bool, HaiderError> {
    if refresh_fences.current(&descriptor.alias) != expected.fence_epoch {
        return Ok(false);
    }
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
    persist_refresh_expired_or_tombstone(
        store,
        accounts,
        vault,
        snapshot,
        management,
        &descriptor.alias,
    )
    .await?;
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
    store: &SqliteStoreHandle,
    accounts: &mut AccountStore<Box<dyn StoreLike>>,
    vault: Arc<dyn Vault>,
    snapshot: &AccountsSnapshot,
    management: Option<&ManagementSnapshot>,
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
        refresh_resolver_snapshot(snapshot, accounts);
        let _ = publish_next_management_revision(store, snapshot, management, accounts).await;
    } else {
        mark_snapshot_expired(snapshot, alias);
    }
    let vault_for_delete = vault;
    let alias_for_delete = alias.clone();
    let tombstoned =
        tokio::task::spawn_blocking(move || vault_for_delete.delete(&alias_for_delete)).await;
    if matches!(tombstoned, Ok(Ok(()))) && !status_persisted {
        let _ = publish_marked_management_revision(store, snapshot, management).await;
    } else if !status_persisted {
        // Both durable barriers failed. The actor's public snapshot remains
        // closed for this process; the command still reports Persist to its
        // caller, and a production test pins the recoverable single-failure
        // boundary (descriptor failure + durable vault tombstone).
        mark_snapshot_expired(snapshot, alias);
    }
}

async fn persist_refresh_expired_or_tombstone(
    store: &SqliteStoreHandle,
    accounts: &mut AccountStore<Box<dyn StoreLike>>,
    vault: Arc<dyn Vault>,
    snapshot: &AccountsSnapshot,
    management: Option<&ManagementSnapshot>,
    alias: &CredentialAlias,
) -> Result<(), HaiderError> {
    match accounts.set_status(alias, CredentialStatus::Expired) {
        Ok(()) => {
            refresh_resolver_snapshot(snapshot, accounts);
            publish_next_management_revision(store, snapshot, management, accounts).await?;
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
                    publish_marked_management_revision(store, snapshot, management).await?;
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

fn refresh_resolver_snapshot(
    snapshot: &AccountsSnapshot,
    accounts: &AccountStore<Box<dyn StoreLike>>,
) {
    if let Ok(mut view) = snapshot.lock() {
        *view = accounts.list().to_vec();
    }
}

fn try_refresh_resolver_snapshot(
    snapshot: &AccountsSnapshot,
    accounts: &AccountStore<Box<dyn StoreLike>>,
) -> Result<(), HaiderError> {
    let mut view = snapshot.lock().map_err(|_| {
        HaiderError::new(
            ErrorCode::Internal,
            "account resolver snapshot is unavailable",
            true,
        )
    })?;
    *view = accounts.list().to_vec();
    Ok(())
}

fn publish_management_snapshot(
    snapshot: &AccountsSnapshot,
    management: Option<&ManagementSnapshot>,
    accounts: &AccountStore<Box<dyn StoreLike>>,
    revision: u64,
) {
    let descriptors = accounts.list().to_vec();
    if let Ok(mut view) = snapshot.lock() {
        *view = descriptors.clone();
    }
    if let Some(management) = management {
        management.publish_accounts(revision, descriptors);
    }
}

async fn publish_next_management_revision(
    store: &SqliteStoreHandle,
    snapshot: &AccountsSnapshot,
    management: Option<&ManagementSnapshot>,
    accounts: &AccountStore<Box<dyn StoreLike>>,
) -> Result<u64, HaiderError> {
    let revision = store.advance_management_revision().await?;
    publish_management_snapshot(snapshot, management, accounts, revision);
    Ok(revision)
}

async fn publish_marked_management_revision(
    store: &SqliteStoreHandle,
    snapshot: &AccountsSnapshot,
    management: Option<&ManagementSnapshot>,
) -> Result<u64, HaiderError> {
    let revision = store.advance_management_revision().await?;
    if let Some(management) = management
        && let Ok(descriptors) = snapshot.lock().map(|view| view.clone())
    {
        management.publish_accounts(revision, descriptors);
    }
    Ok(revision)
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct SetActiveIdentity {
    alias: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct SetActiveRecovery {
    provider: String,
    prior_alias: Option<CredentialAlias>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct SetActiveReceipt {
    descriptor: CredentialDescriptor,
    prior_alias: Option<CredentialAlias>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct RemoveIdentity {
    alias: String,
    expected_revision: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct RemoveRecovery {
    provider: String,
    was_active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct RemoveReceipt {
    removed_alias: CredentialAlias,
    replacement_active_alias: Option<CredentialAlias>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct SetDefaultModelIdentity {
    provider: String,
    model: String,
    expected_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct ProviderConfigureIdentity {
    input: ProviderConfigureInput,
    expected_revision: u64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct ProviderReceipt {
    provider: ProviderSummaryWire,
}

fn command_json<T: serde::Serialize>(value: &T) -> Result<(String, String), HaiderError> {
    let json = serde_json::to_string(value).map_err(|error| {
        HaiderError::new(
            ErrorCode::InvalidArgument,
            format!("cannot encode management command coordinates: {error}"),
            false,
        )
    })?;
    let digest = blake3::hash(json.as_bytes()).to_hex().to_string();
    Ok((json, digest))
}

fn respond_management_error(route: &LoginRoute, error: &HaiderError) {
    let (code, data) = match error.code {
        ErrorCode::RevisionConflict => {
            let expected_revision = error
                .details
                .as_ref()
                .and_then(|details| details.get("expected_revision"))
                .and_then(serde_json::Value::as_u64);
            let current_revision = error
                .details
                .as_ref()
                .and_then(|details| details.get("current_revision"))
                .and_then(serde_json::Value::as_u64);
            let data = expected_revision.zip(current_revision).map(
                |(expected_revision, current_revision)| ErrorData::RevisionConflict {
                    expected_revision,
                    current_revision,
                },
            );
            (ERROR_CODE_REVISION_CONFLICT, data)
        }
        ErrorCode::Busy => (ERROR_CODE_BUSY, None),
        ErrorCode::CredentialMissing => (ERROR_CODE_CREDENTIAL_MISSING, None),
        ErrorCode::InvalidArgument => (ERROR_CODE_INVALID_ARGUMENT, None),
        _ => (ERROR_CODE_PROVIDER_ERROR, None),
    };
    respond(
        route,
        ResponseBody::Error {
            code: code.to_owned(),
            message: error.message.clone(),
            retryable: error.retryable,
            data,
        },
    );
}

async fn check_expected_revision(
    store: &SqliteStoreHandle,
    expected_revision: u64,
) -> Result<(), HaiderError> {
    let current_revision = store.management_revision().await?;
    if expected_revision == current_revision {
        return Ok(());
    }
    let mut error = HaiderError::new(
        ErrorCode::RevisionConflict,
        format!(
            "expected management revision {expected_revision}, current revision is {current_revision}"
        ),
        true,
    );
    error.details = Some(serde_json::json!({
        "expected_revision": expected_revision,
        "current_revision": current_revision,
    }));
    Err(error)
}

#[allow(clippy::too_many_arguments)]
async fn handle_set_active(
    store: &SqliteStoreHandle,
    accounts: &mut AccountStore<Box<dyn StoreLike>>,
    snapshot: &AccountsSnapshot,
    management: Option<&ManagementSnapshot>,
    providers: &ProviderRegistry<Box<dyn ProviderRegistryStoreLike>>,
    job: SetActiveJob,
) {
    let alias = match normalize_account_alias(&job.alias) {
        Ok(alias) => CredentialAlias::new(alias),
        Err(error) => {
            respond_management_error(&job.route, &error);
            return;
        }
    };
    let identity = SetActiveIdentity {
        alias: alias.as_str().to_owned(),
    };
    let (request_json, request_digest) = match command_json(&identity) {
        Ok(value) => value,
        Err(error) => {
            respond_management_error(&job.route, &error);
            return;
        }
    };
    let preflight = store
        .management_receipt_preflight::<SetActiveReceipt>(
            job.command_id.clone(),
            ACCOUNT_SET_ACTIVE_METHOD.to_owned(),
            request_digest.clone(),
            request_json.clone(),
        )
        .await;
    let recovery = match preflight {
        Ok(Some(ManagementClaim::Committed { response, revision })) => {
            respond(
                &job.route,
                ResponseBody::AccountSetActive {
                    descriptor: response.descriptor,
                    prior_alias: response.prior_alias,
                    revision,
                },
            );
            return;
        }
        Ok(Some(ManagementClaim::ResumePending { recovery_json })) => {
            match recovery_json.and_then(|json| serde_json::from_str(&json).ok()) {
                Some(recovery) => recovery,
                None => {
                    respond_management_error(
                        &job.route,
                        &HaiderError::new(
                            ErrorCode::StoreCorrupt,
                            "pending set-active receipt has no recovery coordinates",
                            false,
                        ),
                    );
                    return;
                }
            }
        }
        Ok(Some(ManagementClaim::Fresh)) => unreachable!("preflight never returns Fresh"),
        Ok(None) => {
            let Some(descriptor) = accounts.get(&alias) else {
                respond_management_error(
                    &job.route,
                    &HaiderError::new(
                        ErrorCode::CredentialMissing,
                        format!("credential alias `{alias}` does not exist"),
                        false,
                    ),
                );
                return;
            };
            SetActiveRecovery {
                provider: descriptor.provider.clone(),
                prior_alias: accounts
                    .active_for_provider(&descriptor.provider)
                    .map(|active| active.alias.clone()),
            }
        }
        Err(error) => {
            respond_management_error(&job.route, &error);
            return;
        }
    };
    let recovery_json = match serde_json::to_string(&recovery) {
        Ok(json) => json,
        Err(error) => {
            respond_management_error(
                &job.route,
                &HaiderError::new(ErrorCode::Internal, error.to_string(), false),
            );
            return;
        }
    };
    match store
        .management_claim_receipt::<SetActiveReceipt>(
            job.command_id.clone(),
            ACCOUNT_SET_ACTIVE_METHOD.to_owned(),
            request_digest,
            request_json,
            Some(recovery_json),
            None,
        )
        .await
    {
        Ok(ManagementClaim::Committed { response, revision }) => {
            respond(
                &job.route,
                ResponseBody::AccountSetActive {
                    descriptor: response.descriptor,
                    prior_alias: response.prior_alias,
                    revision,
                },
            );
            return;
        }
        Ok(ManagementClaim::Fresh | ManagementClaim::ResumePending { .. }) => {}
        Err(error) => {
            respond_management_error(&job.route, &error);
            return;
        }
    }
    if accounts
        .get(&alias)
        .is_none_or(|descriptor| descriptor.provider != recovery.provider)
    {
        respond_management_error(
            &job.route,
            &HaiderError::new(
                ErrorCode::CredentialMissing,
                format!("credential alias `{alias}` is no longer available"),
                false,
            ),
        );
        return;
    }
    if let Err(error) = accounts.select(&alias) {
        respond_management_error(&job.route, &error);
        return;
    }
    if let Err(error) = try_refresh_resolver_snapshot(snapshot, accounts) {
        respond_management_error(&job.route, &error);
        return;
    }
    let Some(descriptor) = accounts.get(&alias).cloned() else {
        respond_management_error(
            &job.route,
            &HaiderError::new(
                ErrorCode::CredentialMissing,
                format!("credential alias `{alias}` disappeared after selection"),
                true,
            ),
        );
        return;
    };
    let receipt = SetActiveReceipt {
        descriptor: descriptor.clone(),
        prior_alias: recovery.prior_alias,
    };
    let revision = match store
        .finalize_management_receipt(
            job.command_id,
            ACCOUNT_SET_ACTIVE_METHOD.to_owned(),
            receipt.clone(),
        )
        .await
    {
        Ok(revision) => revision,
        Err(error) => {
            respond_management_error(&job.route, &error);
            return;
        }
    };
    if let Some(management) = management {
        management.publish(revision, accounts.list().to_vec(), providers.summaries());
    }
    respond(
        &job.route,
        ResponseBody::AccountSetActive {
            descriptor,
            prior_alias: receipt.prior_alias,
            revision,
        },
    );
}

#[allow(clippy::too_many_arguments)]
async fn handle_remove_account(
    store: &SqliteStoreHandle,
    accounts: &mut AccountStore<Box<dyn StoreLike>>,
    vault: Arc<dyn Vault>,
    snapshot: &AccountsSnapshot,
    management: Option<&ManagementSnapshot>,
    providers: &ProviderRegistry<Box<dyn ProviderRegistryStoreLike>>,
    reserved_aliases: &mut HashSet<String>,
    refresh_fences: &RefreshFenceRegistry,
    job: RemoveAccountJob,
) {
    let alias = match normalize_account_alias(&job.alias) {
        Ok(alias) => CredentialAlias::new(alias),
        Err(error) => {
            respond_management_error(&job.route, &error);
            return;
        }
    };
    let identity = RemoveIdentity {
        alias: alias.as_str().to_owned(),
        expected_revision: job.expected_revision,
    };
    let (request_json, request_digest) = match command_json(&identity) {
        Ok(value) => value,
        Err(error) => {
            respond_management_error(&job.route, &error);
            return;
        }
    };
    let preflight = store
        .management_receipt_preflight::<RemoveReceipt>(
            job.command_id.clone(),
            ACCOUNT_REMOVE_METHOD.to_owned(),
            request_digest.clone(),
            request_json.clone(),
        )
        .await;
    if matches!(&preflight, Ok(None))
        && let Some(expected_revision) = job.expected_revision
        && let Err(error) = check_expected_revision(store, expected_revision).await
    {
        respond_management_error(&job.route, &error);
        return;
    }
    let recovery = match preflight {
        Ok(Some(ManagementClaim::Committed { response, revision })) => {
            respond(
                &job.route,
                ResponseBody::AccountRemove {
                    removed_alias: response.removed_alias,
                    replacement_active_alias: response.replacement_active_alias,
                    revision,
                },
            );
            return;
        }
        Ok(Some(ManagementClaim::ResumePending { recovery_json })) => {
            match recovery_json.and_then(|json| serde_json::from_str(&json).ok()) {
                Some(recovery) => recovery,
                None => {
                    respond_management_error(
                        &job.route,
                        &HaiderError::new(
                            ErrorCode::StoreCorrupt,
                            "pending remove receipt has no recovery coordinates",
                            false,
                        ),
                    );
                    return;
                }
            }
        }
        Ok(Some(ManagementClaim::Fresh)) => unreachable!("preflight never returns Fresh"),
        Ok(None) => {
            let Some(descriptor) = accounts.get(&alias) else {
                respond_management_error(
                    &job.route,
                    &HaiderError::new(
                        ErrorCode::CredentialMissing,
                        format!("credential alias `{alias}` does not exist"),
                        false,
                    ),
                );
                return;
            };
            RemoveRecovery {
                provider: descriptor.provider.clone(),
                was_active: descriptor.active,
            }
        }
        Err(error) => {
            respond_management_error(&job.route, &error);
            return;
        }
    };
    let recovery_json = match serde_json::to_string(&recovery) {
        Ok(recovery_json) => recovery_json,
        Err(error) => {
            respond_management_error(
                &job.route,
                &HaiderError::new(
                    ErrorCode::Internal,
                    format!("could not encode remove recovery coordinates: {error}"),
                    false,
                ),
            );
            return;
        }
    };
    match store
        .account_remove_claim_receipt::<RemoveReceipt>(
            job.command_id.clone(),
            request_digest,
            request_json,
            recovery_json,
            job.expected_revision,
            alias.as_str().to_owned(),
            recovery.provider.clone(),
            recovery.was_active,
        )
        .await
    {
        Ok(ManagementClaim::Committed { response, revision }) => {
            respond(
                &job.route,
                ResponseBody::AccountRemove {
                    removed_alias: response.removed_alias,
                    replacement_active_alias: response.replacement_active_alias,
                    revision,
                },
            );
            return;
        }
        Ok(ManagementClaim::Fresh | ManagementClaim::ResumePending { .. }) => {}
        Err(error) => {
            respond_management_error(&job.route, &error);
            return;
        }
    }
    reserved_aliases.insert(alias.as_str().to_owned());
    // The exchange runs outside this actor. Advancing the shared epoch before
    // descriptor publication and deletion makes a late completion stale even
    // if the same alias is later re-added with identical durable metadata.
    refresh_fences.invalidate(&alias);
    if let Some(descriptor) = accounts.get(&alias) {
        if descriptor.provider != recovery.provider {
            respond_management_error(
                &job.route,
                &HaiderError::new(
                    ErrorCode::StoreCorrupt,
                    "reserved remove alias changed provider",
                    false,
                ),
            );
            return;
        }
        if let Err(error) = accounts.remove(&alias) {
            respond_management_error(&job.route, &error);
            return;
        }
    }
    // Resolver publication deliberately precedes vault deletion. The public
    // management snapshot waits for the receipt/revision transaction.
    if let Err(error) = try_refresh_resolver_snapshot(snapshot, accounts) {
        respond_management_error(&job.route, &error);
        return;
    }
    let replacement_active_alias = accounts
        .active_for_provider(&recovery.provider)
        .map(|descriptor| descriptor.alias.clone());
    let alias_for_delete = alias.clone();
    let delete = tokio::task::spawn_blocking(move || vault.delete(&alias_for_delete)).await;
    match delete {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            respond_management_error(&job.route, &error);
            return;
        }
        Err(_) => {
            respond_management_error(
                &job.route,
                &HaiderError::new(
                    ErrorCode::Internal,
                    "account vault deletion worker was lost",
                    true,
                ),
            );
            return;
        }
    }
    let receipt = RemoveReceipt {
        removed_alias: alias.clone(),
        replacement_active_alias: replacement_active_alias.clone(),
    };
    let revision = match store
        .finalize_account_remove_receipt(job.command_id, receipt)
        .await
    {
        Ok(revision) => revision,
        Err(error) => {
            respond_management_error(&job.route, &error);
            return;
        }
    };
    reserved_aliases.remove(alias.as_str());
    if let Some(management) = management {
        management.publish(revision, accounts.list().to_vec(), providers.summaries());
    }
    respond(
        &job.route,
        ResponseBody::AccountRemove {
            removed_alias: alias,
            replacement_active_alias,
            revision,
        },
    );
}

async fn handle_set_default_model(
    store: &SqliteStoreHandle,
    accounts: &AccountStore<Box<dyn StoreLike>>,
    management: Option<&ManagementSnapshot>,
    providers: &mut ProviderRegistry<Box<dyn ProviderRegistryStoreLike>>,
    job: SetDefaultModelJob,
) {
    let identity = SetDefaultModelIdentity {
        provider: job.provider.clone(),
        model: job.model.trim().to_owned(),
        expected_revision: job.expected_revision,
    };
    let (request_json, request_digest) = match command_json(&identity) {
        Ok(value) => value,
        Err(error) => {
            respond_management_error(&job.route, &error);
            return;
        }
    };
    let preflight = store
        .management_receipt_preflight::<ProviderReceipt>(
            job.command_id.clone(),
            ACCOUNT_SET_DEFAULT_MODEL_METHOD.to_owned(),
            request_digest.clone(),
            request_json.clone(),
        )
        .await;
    if matches!(&preflight, Ok(None))
        && let Err(error) = check_expected_revision(store, job.expected_revision).await
    {
        respond_management_error(&job.route, &error);
        return;
    }
    match preflight {
        Ok(Some(ManagementClaim::Committed { response, revision })) => {
            respond(
                &job.route,
                ResponseBody::AccountSetDefaultModel {
                    provider: response.provider,
                    revision,
                },
            );
            return;
        }
        Ok(_) => {}
        Err(error) => {
            respond_management_error(&job.route, &error);
            return;
        }
    }
    if let Err(error) = providers.validate_default_model(&identity.provider, &identity.model) {
        respond_management_error(&job.route, &error);
        return;
    }
    match store
        .management_claim_receipt::<ProviderReceipt>(
            job.command_id.clone(),
            ACCOUNT_SET_DEFAULT_MODEL_METHOD.to_owned(),
            request_digest,
            request_json,
            None,
            Some(job.expected_revision),
        )
        .await
    {
        Ok(ManagementClaim::Committed { response, revision }) => {
            respond(
                &job.route,
                ResponseBody::AccountSetDefaultModel {
                    provider: response.provider,
                    revision,
                },
            );
            return;
        }
        Ok(ManagementClaim::Fresh | ManagementClaim::ResumePending { .. }) => {}
        Err(error) => {
            respond_management_error(&job.route, &error);
            return;
        }
    }
    let profile = match providers.set_default_model(&identity.provider, &identity.model) {
        Ok(profile) => profile,
        Err(error) => {
            respond_management_error(&job.route, &error);
            return;
        }
    };
    let Some(provider) = providers.summary(&profile.provider_id) else {
        respond_management_error(
            &job.route,
            &HaiderError::new(
                ErrorCode::StoreCorrupt,
                "configured provider disappeared before receipt finalization",
                false,
            ),
        );
        return;
    };
    let receipt = ProviderReceipt { provider };
    let revision = match store
        .finalize_management_receipt(
            job.command_id,
            ACCOUNT_SET_DEFAULT_MODEL_METHOD.to_owned(),
            receipt.clone(),
        )
        .await
    {
        Ok(revision) => revision,
        Err(error) => {
            respond_management_error(&job.route, &error);
            return;
        }
    };
    if let Some(management) = management {
        management.publish(revision, accounts.list().to_vec(), providers.summaries());
    }
    respond(
        &job.route,
        ResponseBody::AccountSetDefaultModel {
            provider: receipt.provider,
            revision,
        },
    );
}

async fn handle_provider_configure(
    store: &SqliteStoreHandle,
    accounts: &AccountStore<Box<dyn StoreLike>>,
    management: Option<&ManagementSnapshot>,
    providers: &mut ProviderRegistry<Box<dyn ProviderRegistryStoreLike>>,
    endpoint_validator: Arc<dyn ProviderEndpointValidator>,
    mut job: ProviderConfigureJob,
) {
    let identity = ProviderConfigureIdentity {
        input: job.input.clone(),
        expected_revision: job.expected_revision,
    };
    let (request_json, request_digest) = match command_json(&identity) {
        Ok(value) => value,
        Err(error) => {
            respond_management_error(&job.route, &error);
            return;
        }
    };
    let preflight = store
        .management_receipt_preflight::<ProviderReceipt>(
            job.command_id.clone(),
            PROVIDER_CONFIGURE_METHOD.to_owned(),
            request_digest.clone(),
            request_json.clone(),
        )
        .await;
    if matches!(&preflight, Ok(None))
        && let Err(error) = check_expected_revision(store, job.expected_revision).await
    {
        respond_management_error(&job.route, &error);
        return;
    }
    match preflight {
        Ok(Some(ManagementClaim::Committed { response, revision })) => {
            respond(
                &job.route,
                ResponseBody::ProviderConfigure {
                    provider: response.provider,
                    revision,
                },
            );
            return;
        }
        Ok(_) => {}
        Err(error) => {
            respond_management_error(&job.route, &error);
            return;
        }
    }
    if let Err(error) = providers.validate_configure(job.input.clone()) {
        respond_management_error(&job.route, &error);
        return;
    }
    if providers.get(&job.input.provider).is_none() {
        let Some(origin) = job.input.origin.as_deref() else {
            respond_management_error(
                &job.route,
                &HaiderError::new(
                    ErrorCode::InvalidArgument,
                    "new provider configuration requires an origin",
                    false,
                ),
            );
            return;
        };
        let origin = origin.to_owned();
        let validation =
            tokio::spawn(async move { endpoint_validator.validate(&origin).await }).await;
        match validation {
            Ok(Ok(canonical_origin)) => job.input.origin = Some(canonical_origin),
            Ok(Err(error)) => {
                respond_management_error(&job.route, &error);
                return;
            }
            Err(_) => {
                respond_management_error(
                    &job.route,
                    &HaiderError::new(
                        ErrorCode::Internal,
                        "provider endpoint validation worker was lost",
                        true,
                    ),
                );
                return;
            }
        }
    }
    let recovery_json = match serde_json::to_string(&job.input) {
        Ok(recovery_json) => recovery_json,
        Err(error) => {
            respond_management_error(
                &job.route,
                &HaiderError::new(
                    ErrorCode::Internal,
                    format!("could not encode provider recovery coordinates: {error}"),
                    false,
                ),
            );
            return;
        }
    };
    match store
        .management_claim_receipt::<ProviderReceipt>(
            job.command_id.clone(),
            PROVIDER_CONFIGURE_METHOD.to_owned(),
            request_digest,
            request_json,
            Some(recovery_json),
            Some(job.expected_revision),
        )
        .await
    {
        Ok(ManagementClaim::Committed { response, revision }) => {
            respond(
                &job.route,
                ResponseBody::ProviderConfigure {
                    provider: response.provider,
                    revision,
                },
            );
            return;
        }
        Ok(ManagementClaim::ResumePending {
            recovery_json: Some(recovery),
        }) => {
            if let Ok(input) = serde_json::from_str(&recovery) {
                job.input = input;
            }
        }
        Ok(ManagementClaim::Fresh | ManagementClaim::ResumePending { .. }) => {}
        Err(error) => {
            respond_management_error(&job.route, &error);
            return;
        }
    }
    let profile = match providers.configure(job.input) {
        Ok(profile) => profile,
        Err(error) => {
            respond_management_error(&job.route, &error);
            return;
        }
    };
    let Some(provider) = providers.summary(&profile.provider_id) else {
        respond_management_error(
            &job.route,
            &HaiderError::new(
                ErrorCode::StoreCorrupt,
                "configured provider disappeared before receipt finalization",
                false,
            ),
        );
        return;
    };
    let receipt = ProviderReceipt { provider };
    let revision = match store
        .finalize_management_receipt(
            job.command_id,
            PROVIDER_CONFIGURE_METHOD.to_owned(),
            receipt.clone(),
        )
        .await
    {
        Ok(revision) => revision,
        Err(error) => {
            respond_management_error(&job.route, &error);
            return;
        }
    };
    if let Some(management) = management {
        management.publish(revision, accounts.list().to_vec(), providers.summaries());
    }
    respond(
        &job.route,
        ResponseBody::ProviderConfigure {
            provider: receipt.provider,
            revision,
        },
    );
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
    management: Option<&ManagementSnapshot>,
    _profile_id: &str,
    default_model: &str,
    pending: &mut HashMap<String, PendingSecret>,
    reserved_aliases: &HashSet<String>,
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
    let selected_alias = match display_alias {
        Some(alias) => match normalize_account_alias(&alias) {
            Ok(alias) => alias,
            Err(error) => {
                respond_error(&route, ERROR_CODE_INVALID_ARGUMENT, &error.message, false);
                return;
            }
        },
        None => canonical_api_alias(&provider, &command_id),
    };
    let identity = LoginIdentity {
        provider: provider.clone(),
        resolved_model: validation_model.unwrap_or_else(|| default_model.to_owned()),
        display_alias: Some(selected_alias.clone()),
        physical_alias: selected_alias,
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
    if reserved_aliases.contains(alias.as_str()) {
        if let Some(secret) = secret {
            pending.insert(
                command_id,
                PendingSecret {
                    secret,
                    claimed_at: Instant::now(),
                },
            );
        }
        respond_error(
            &route,
            ERROR_CODE_BUSY,
            "account alias is reserved by pending removal cleanup",
            true,
        );
        return;
    }
    if resume {
        // Crash-boundary reconciliation at command time (R10 step 10):
        // descriptor present -> finalize; vault-only -> resume descriptor
        // commit; neither -> continue with a fresh stage below.
        if accounts.get(&alias).is_some() {
            drop(secret);
            pending.remove(&command_id);
            finalize_and_respond(
                store,
                accounts,
                snapshot,
                management,
                &command_id,
                &alias,
                &route,
            )
            .await;
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
            finalize_and_respond(
                store,
                accounts,
                snapshot,
                management,
                &command_id,
                &alias,
                &route,
            )
            .await;
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
            finalize_and_respond(
                store,
                accounts,
                snapshot,
                management,
                &command_id,
                &alias,
                &route,
            )
            .await;
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

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct OAuthImportIdentity {
    source: String,
    alias: String,
    provider: String,
}

impl OAuthImportIdentity {
    fn canonical_json(&self) -> Result<String, HaiderError> {
        serde_json::to_string(self).map_err(|error| {
            HaiderError::new(
                ErrorCode::InvalidArgument,
                format!("cannot encode OAuth import coordinates: {error}"),
                false,
            )
        })
    }
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum OAuthReceiptIdentity {
    Import(OAuthImportIdentity),
    Add(OAuthAddIdentity),
}

impl OAuthReceiptIdentity {
    fn provider(&self) -> &str {
        match self {
            Self::Import(identity) => &identity.provider,
            Self::Add(identity) => &identity.provider,
        }
    }

    fn alias(&self) -> &str {
        match self {
            Self::Import(identity) => &identity.alias,
            Self::Add(identity) => &identity.physical_alias,
        }
    }
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
#[allow(clippy::too_many_arguments)]
async fn handle_oauth_add(
    store: &SqliteStoreHandle,
    accounts: &mut AccountStore<Box<dyn StoreLike>>,
    vault: Arc<dyn Vault>,
    snapshot: &AccountsSnapshot,
    management: Option<&ManagementSnapshot>,
    _profile_id: &str,
    reserved_aliases: &HashSet<String>,
    job: OAuthAddJob,
) {
    let OAuthAddJob {
        command_id,
        provider,
        display_alias,
        claim,
        route,
    } = job;
    let display_alias = match normalize_account_alias(&display_alias) {
        Ok(alias) => alias,
        Err(error) => {
            respond_error(&route, ERROR_CODE_INVALID_ARGUMENT, &error.message, false);
            return;
        }
    };
    let identity = OAuthAddIdentity {
        provider: provider.clone(),
        physical_alias: display_alias.clone(),
        display_alias,
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
    if reserved_aliases.contains(alias.as_str()) {
        respond_error(
            &route,
            ERROR_CODE_BUSY,
            "account alias is reserved by pending removal cleanup",
            true,
        );
        return;
    }
    if resume && accounts.get(&alias).is_some() {
        finalize_oauth_commit(
            store,
            accounts,
            snapshot,
            management,
            &command_id,
            &alias,
            &route,
            OAuthCommitResponse::Add,
        )
        .await;
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
                    let descriptor = oauth_descriptor_for(
                        &identity.provider,
                        &alias,
                        &bundle,
                        accounts.active_for_provider(&identity.provider).is_none(),
                    );
                    if let Err(error) = accounts.add(descriptor) {
                        respond_error(
                            &route,
                            ERROR_CODE_PROVIDER_ERROR,
                            &format!("resumed OAuth descriptor commit failed: {}", error.message),
                            true,
                        );
                        return;
                    }
                    finalize_oauth_commit(
                        store,
                        accounts,
                        snapshot,
                        management,
                        &command_id,
                        &alias,
                        &route,
                        OAuthCommitResponse::Add,
                    )
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
    if let Err(error) = persist_oauth_bundle(
        accounts,
        Arc::clone(&vault),
        &identity.provider,
        &alias,
        claim.bundle,
        None,
    )
    .await
    {
        respond_error(
            &route,
            ERROR_CODE_PROVIDER_ERROR,
            &error.message,
            error.retryable,
        );
        return;
    }
    finalize_oauth_commit(
        store,
        accounts,
        snapshot,
        management,
        &command_id,
        &alias,
        &route,
        OAuthCommitResponse::Add,
    )
    .await;
}

const ACCOUNT_ADD_RECEIPT_METHOD: &str = "account.add";

#[allow(clippy::too_many_arguments)]
async fn handle_oauth_import(
    store: &SqliteStoreHandle,
    accounts: &mut AccountStore<Box<dyn StoreLike>>,
    vault: Arc<dyn Vault>,
    snapshot: &AccountsSnapshot,
    management: Option<&ManagementSnapshot>,
    reserved_aliases: &HashSet<String>,
    refresh_fences: &RefreshFenceRegistry,
    job: OAuthImportJob,
) {
    let spec = match oauth_import_source_spec(&job.source) {
        Ok(spec) => spec,
        Err(error) => {
            respond_management_error(&job.route, &error);
            return;
        }
    };
    let alias = match select_oauth_import_alias(
        store,
        accounts,
        &job.command_id,
        spec.source,
        spec.provider,
        spec.default_alias,
    )
    .await
    {
        Ok(alias) => alias,
        Err(error) => {
            respond_management_error(&job.route, &error);
            return;
        }
    };
    let identity = OAuthImportIdentity {
        source: spec.source.to_owned(),
        alias: alias.as_str().to_owned(),
        provider: spec.provider.to_owned(),
    };
    let request_json = match identity.canonical_json() {
        Ok(json) => json,
        Err(error) => {
            respond_management_error(&job.route, &error);
            return;
        }
    };
    let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
    match store
        .management_receipt_preflight::<AccountAddReceiptResponse>(
            job.command_id.clone(),
            ACCOUNT_ADD_RECEIPT_METHOD.to_owned(),
            request_digest.clone(),
            request_json.clone(),
        )
        .await
    {
        Ok(Some(ManagementClaim::Committed { response, revision })) => {
            respond(
                &job.route,
                ResponseBody::AccountOAuthImport {
                    descriptor: response.descriptor,
                    revision,
                },
            );
            return;
        }
        Ok(Some(ManagementClaim::ResumePending { .. })) | Ok(None) => {}
        Ok(Some(ManagementClaim::Fresh)) => unreachable!("preflight never returns Fresh"),
        Err(error) => {
            respond_management_error(&job.route, &error);
            return;
        }
    }
    let resume = match store
        .account_add_claim_receipt(job.command_id.clone(), request_digest, request_json)
        .await
    {
        Ok(AccountAddClaim::Fresh) => false,
        Ok(AccountAddClaim::ResumePending) => true,
        Ok(AccountAddClaim::Committed(response)) => {
            let revision = match store.account_add_receipts().await.ok().and_then(|rows| {
                rows.into_iter()
                    .find(|row| row.command_id == job.command_id)
                    .and_then(|row| row.final_revision)
            }) {
                Some(revision) => revision,
                None => {
                    respond_management_error(
                        &job.route,
                        &HaiderError::new(
                            ErrorCode::StoreCorrupt,
                            "committed OAuth import receipt has no revision",
                            false,
                        ),
                    );
                    return;
                }
            };
            respond(
                &job.route,
                ResponseBody::AccountOAuthImport {
                    descriptor: response.descriptor,
                    revision,
                },
            );
            return;
        }
        Err(error) => {
            respond_management_error(&job.route, &error);
            return;
        }
    };
    if reserved_aliases.contains(alias.as_str()) {
        respond_management_error(
            &job.route,
            &HaiderError::new(
                ErrorCode::Busy,
                "account alias is reserved by pending removal cleanup",
                true,
            ),
        );
        return;
    }
    match alias_has_pending_login_reservation(store, &alias).await {
        Ok(false) => {}
        Ok(true) => {
            respond_management_error(
                &job.route,
                &HaiderError::new(
                    ErrorCode::Busy,
                    "account alias is reserved by a pending login",
                    true,
                ),
            );
            return;
        }
        Err(error) => {
            respond_management_error(&job.route, &error);
            return;
        }
    }
    let replacing = accounts.get(&alias).cloned();
    if replacing.as_ref().is_some_and(|descriptor| {
        descriptor.provider != spec.provider || descriptor.auth_method != AuthMethod::OAuth
    }) {
        respond_management_error(
            &job.route,
            &HaiderError::new(
                ErrorCode::InvalidArgument,
                format!(
                    "credential alias `{alias}` is not a replaceable `{}` OAuth account",
                    spec.provider
                ),
                false,
            ),
        );
        return;
    }
    let prior_secret = if replacing.is_some() {
        let vault_for_read = Arc::clone(&vault);
        let alias_for_read = alias.clone();
        match tokio::task::spawn_blocking(move || vault_for_read.resolve(&alias_for_read)).await {
            Ok(Ok(stored)) => Some(stored),
            Ok(Err(error)) => {
                respond_management_error(&job.route, &error);
                return;
            }
            Err(_) => {
                respond_management_error(
                    &job.route,
                    &HaiderError::new(ErrorCode::ProviderError, "OAuth vault worker failed", true),
                );
                return;
            }
        }
    } else {
        None
    };
    let prior_bundle = match prior_secret.as_ref() {
        Some(stored) => match haider_accounts::OAuthTokenBundleV1::decode(stored.expose_secret()) {
            Ok(bundle) if bundle.provider_id == spec.provider => Some(bundle),
            _ => {
                respond_management_error(
                    &job.route,
                    &HaiderError::new(
                        ErrorCode::Unauthorized,
                        "stored OAuth token bundle is invalid; remove the account and import again",
                        false,
                    ),
                );
                return;
            }
        },
        None => None,
    };
    let generation = match prior_bundle.as_ref() {
        Some(bundle) => match bundle.generation.checked_add(1) {
            Some(generation) => generation,
            None => {
                respond_management_error(
                    &job.route,
                    &HaiderError::new(
                        ErrorCode::ProviderError,
                        "OAuth token generation is exhausted",
                        false,
                    ),
                );
                return;
            }
        },
        None => 1,
    };
    let source = spec.source.to_owned();
    let imported =
        match tokio::task::spawn_blocking(move || load_oauth_import_bundle(&source, generation))
            .await
        {
            Ok(Ok(bundle)) => bundle,
            Ok(Err(error)) => {
                respond_management_error(&job.route, &error);
                return;
            }
            Err(_) => {
                respond_management_error(
                    &job.route,
                    &HaiderError::new(
                        ErrorCode::ProviderError,
                        "OAuth import file worker failed",
                        true,
                    ),
                );
                return;
            }
        };
    if imported.provider_id != spec.provider {
        respond_management_error(
            &job.route,
            &HaiderError::new(
                ErrorCode::InvalidArgument,
                "OAuth import source resolved to the wrong provider",
                false,
            ),
        );
        return;
    }
    if resume
        && let Some(prior) = prior_bundle.as_ref()
        && same_oauth_import(prior, &imported)
    {
        let descriptor = oauth_descriptor_for(
            spec.provider,
            &alias,
            prior,
            replacing
                .as_ref()
                .is_some_and(|descriptor| descriptor.active)
                || accounts.active_for_provider(spec.provider).is_none(),
        );
        let committed = if replacing.is_some() {
            accounts.replace(descriptor)
        } else {
            accounts.add(descriptor)
        };
        if let Err(error) = committed {
            respond_management_error(&job.route, &error);
            return;
        }
        finalize_oauth_commit(
            store,
            accounts,
            snapshot,
            management,
            &job.command_id,
            &alias,
            &job.route,
            OAuthCommitResponse::Import,
        )
        .await;
        return;
    }
    if replacing.is_some() {
        refresh_fences.invalidate(&alias);
    }
    if let Err(error) = persist_oauth_bundle(
        accounts,
        Arc::clone(&vault),
        spec.provider,
        &alias,
        imported,
        prior_secret,
    )
    .await
    {
        respond_management_error(&job.route, &error);
        return;
    }
    finalize_oauth_commit(
        store,
        accounts,
        snapshot,
        management,
        &job.command_id,
        &alias,
        &job.route,
        OAuthCommitResponse::Import,
    )
    .await;
}

async fn select_oauth_import_alias(
    store: &SqliteStoreHandle,
    accounts: &AccountStore<Box<dyn StoreLike>>,
    command_id: &str,
    source: &str,
    provider: &str,
    default_alias: &str,
) -> Result<CredentialAlias, HaiderError> {
    let receipts = store.account_add_receipts().await?;
    if let Some(identity) = receipts
        .iter()
        .find(|row| row.command_id == command_id)
        .and_then(|row| serde_json::from_str::<OAuthImportIdentity>(&row.request_json).ok())
    {
        return normalize_account_alias(&identity.alias).map(CredentialAlias::new);
    }
    // A historical import receipt is not enough to prove that the current
    // descriptor still belongs to that source: the alias may have been
    // removed and later reused by a loopback OAuth add. The latest committed
    // account.add revision for each alias is the incarnation authority.
    let mut prior_aliases = latest_oauth_receipt_identities(&receipts)
        .into_values()
        .filter_map(|(_, identity)| match identity {
            OAuthReceiptIdentity::Import(identity)
                if identity.source == source
                    && identity.provider == provider
                    && accounts
                        .get(&CredentialAlias::new(&identity.alias))
                        .is_some_and(|descriptor| {
                            descriptor.provider == provider
                                && descriptor.auth_method == AuthMethod::OAuth
                        }) =>
            {
                Some(identity.alias)
            }
            OAuthReceiptIdentity::Import(_) | OAuthReceiptIdentity::Add(_) => None,
        })
        .collect::<Vec<_>>();
    prior_aliases.sort_by_key(|alias| oauth_alias_rank(default_alias, alias));
    if let Some(alias) = prior_aliases.into_iter().next() {
        return Ok(CredentialAlias::new(alias));
    }
    let mut candidate = default_alias.to_owned();
    let mut suffix = 1_u32;
    while accounts
        .list()
        .iter()
        .any(|descriptor| descriptor.alias.as_str() == candidate)
    {
        suffix = suffix.checked_add(1).ok_or_else(|| {
            HaiderError::new(
                ErrorCode::InvalidArgument,
                "OAuth import alias suffix space is exhausted",
                false,
            )
        })?;
        candidate = format!("{default_alias}-{suffix}");
    }
    Ok(CredentialAlias::new(candidate))
}

fn latest_oauth_receipt_identities(
    receipts: &[haider_core::AccountAddReceiptRow],
) -> HashMap<String, (u64, OAuthReceiptIdentity)> {
    let mut latest_by_alias = HashMap::new();
    for row in receipts.iter().filter(|row| row.state == "committed") {
        let (Some(revision), Ok(identity)) = (
            row.final_revision,
            serde_json::from_str::<OAuthReceiptIdentity>(&row.request_json),
        ) else {
            continue;
        };
        let alias = identity.alias().to_owned();
        if latest_by_alias
            .get(&alias)
            .is_none_or(|(current, _)| revision > *current)
        {
            latest_by_alias.insert(alias, (revision, identity));
        }
    }
    latest_by_alias
}

async fn alias_has_pending_login_reservation(
    store: &SqliteStoreHandle,
    alias: &CredentialAlias,
) -> Result<bool, HaiderError> {
    for row in store
        .login_receipts()
        .await?
        .into_iter()
        .filter(|row| row.state == "pending")
    {
        let identity: LoginIdentity = serde_json::from_str(&row.request_json).map_err(|error| {
            HaiderError::new(
                ErrorCode::StoreCorrupt,
                format!(
                    "pending login receipt {} has undecodable request coordinates: {error}",
                    row.command_id
                ),
                false,
            )
        })?;
        if identity.physical_alias == alias.as_str() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn oauth_alias_rank(default_alias: &str, alias: &str) -> u32 {
    if alias == default_alias {
        return 0;
    }
    alias
        .strip_prefix(default_alias)
        .and_then(|suffix| suffix.strip_prefix('-'))
        .and_then(|suffix| suffix.parse::<u32>().ok())
        .unwrap_or(u32::MAX)
}

fn same_oauth_import(
    prior: &haider_accounts::OAuthTokenBundleV1,
    imported: &haider_accounts::OAuthTokenBundleV1,
) -> bool {
    prior.provider_id == imported.provider_id
        && prior.issuer == imported.issuer
        && prior.audience == imported.audience
        && prior.resource == imported.resource
        && prior.token_type.eq_ignore_ascii_case(&imported.token_type)
        && prior.granted_scopes == imported.granted_scopes
        && bool::from(prior.access_token().ct_eq(imported.access_token()))
        && match (prior.refresh_token(), imported.refresh_token()) {
            (Some(prior), Some(imported)) => bool::from(prior.ct_eq(imported)),
            (None, None) => true,
            _ => false,
        }
}

async fn persist_oauth_bundle(
    accounts: &mut AccountStore<Box<dyn StoreLike>>,
    vault: Arc<dyn Vault>,
    provider: &str,
    alias: &CredentialAlias,
    bundle: haider_accounts::OAuthTokenBundleV1,
    prior_secret: Option<SecretHandle>,
) -> Result<(), HaiderError> {
    let replacing = prior_secret.is_some();
    let active = accounts.get(alias).map_or_else(
        || accounts.active_for_provider(provider).is_none(),
        |descriptor| descriptor.active,
    );
    let descriptor = oauth_descriptor_for(provider, alias, &bundle, active);
    let encoded = bundle.encode()?;
    let vault_for_put = Arc::clone(&vault);
    let alias_for_put = alias.clone();
    tokio::task::spawn_blocking(move || vault_for_put.put(&alias_for_put, &encoded))
        .await
        .map_err(|_| {
            HaiderError::new(ErrorCode::ProviderError, "OAuth vault worker failed", true)
        })??;
    let descriptor_result = if replacing {
        accounts.replace(descriptor)
    } else {
        accounts.add(descriptor)
    };
    if let Err(error) = descriptor_result {
        let vault_for_rollback = Arc::clone(&vault);
        let alias_for_rollback = alias.clone();
        let rollback = tokio::task::spawn_blocking(move || match prior_secret {
            Some(previous) => vault_for_rollback.put(&alias_for_rollback, previous.expose_secret()),
            None => vault_for_rollback.delete(&alias_for_rollback),
        })
        .await;
        if !matches!(rollback, Ok(Ok(()))) {
            return Err(HaiderError::new(
                ErrorCode::ProviderError,
                "OAuth descriptor save and vault rollback failed",
                true,
            ));
        }
        return Err(HaiderError::new(
            ErrorCode::ProviderError,
            format!("OAuth descriptor save failed: {}", error.message),
            true,
        ));
    }
    Ok(())
}

#[cfg_attr(not(test), allow(dead_code))]
fn oauth_descriptor(
    identity: &OAuthAddIdentity,
    alias: &CredentialAlias,
    bundle: &haider_accounts::OAuthTokenBundleV1,
) -> CredentialDescriptor {
    oauth_descriptor_for(&identity.provider, alias, bundle, true)
}

fn oauth_descriptor_for(
    provider: &str,
    alias: &CredentialAlias,
    bundle: &haider_accounts::OAuthTokenBundleV1,
    active: bool,
) -> CredentialDescriptor {
    CredentialDescriptor {
        alias: alias.clone(),
        provider: provider.to_owned(),
        base_url: None,
        auth_method: AuthMethod::OAuth,
        identity: bundle.identity.display_identity.clone(),
        status: CredentialStatus::Ok,
        active,
    }
}

#[derive(Clone, Copy)]
enum OAuthCommitResponse {
    Add,
    Import,
}

#[allow(clippy::too_many_arguments)]
async fn finalize_oauth_commit(
    store: &SqliteStoreHandle,
    accounts: &mut AccountStore<Box<dyn StoreLike>>,
    snapshot: &AccountsSnapshot,
    management: Option<&ManagementSnapshot>,
    command_id: &str,
    alias: &CredentialAlias,
    route: &LoginRoute,
    response: OAuthCommitResponse,
) {
    let Some(descriptor) = accounts.get(alias).cloned() else {
        let error = HaiderError::new(
            ErrorCode::ProviderError,
            "OAuth descriptor disappeared before receipt finalization",
            true,
        );
        match response {
            OAuthCommitResponse::Add => {
                respond_error(route, ERROR_CODE_PROVIDER_ERROR, &error.message, true);
            }
            OAuthCommitResponse::Import => respond_management_error(route, &error),
        }
        return;
    };
    let revision = match store
        .finalize_account_add_receipt(
            command_id.to_owned(),
            AccountAddReceiptResponse {
                descriptor: descriptor.clone(),
            },
        )
        .await
    {
        Ok(revision) => revision,
        Err(error) => {
            match response {
                OAuthCommitResponse::Add => {
                    respond_error(route, ERROR_CODE_PROVIDER_ERROR, &error.message, true);
                }
                OAuthCommitResponse::Import => respond_management_error(route, &error),
            }
            return;
        }
    };
    publish_management_snapshot(snapshot, management, accounts, revision);
    match response {
        OAuthCommitResponse::Add => respond(route, ResponseBody::AccountAdd { descriptor }),
        OAuthCommitResponse::Import => respond(
            route,
            ResponseBody::AccountOAuthImport {
                descriptor,
                revision,
            },
        ),
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
        base_url: None,
        auth_method: AuthMethod::ApiKey,
        identity: validated_identity.unwrap_or_else(|| "api-key login".into()),
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
    management: Option<&ManagementSnapshot>,
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
    let response = LoginReceiptResponse {
        descriptor: descriptor.clone(),
    };
    let revision = match store
        .finalize_login_receipt(command_id.to_owned(), response)
        .await
    {
        Ok(revision) => revision,
        Err(error) => {
            // The external commit is real (vault + descriptor); the receipt
            // stays pending and the old management snapshot remains
            // published until pre-ready reconciliation finalizes it.
            respond_error(route, ERROR_CODE_PROVIDER_ERROR, &error.message, true);
            return;
        }
    };
    publish_management_snapshot(snapshot, management, accounts, revision);
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
    let reserved = store
        .reserved_account_aliases()
        .await?
        .into_iter()
        .collect::<HashSet<_>>();
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
        if reserved.contains(alias.as_str()) {
            continue;
        }
        match row.state.as_str() {
            "committed" => {
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
                if accounts.get(&alias).is_none() {
                    let mut descriptor = response.descriptor;
                    // Self-heal without stealing active from a later login.
                    descriptor.active =
                        accounts.active_for_provider(&descriptor.provider).is_none();
                    accounts.add(descriptor)?;
                }
                if row.final_revision.is_none() {
                    store
                        .ensure_committed_management_revision(
                            row.command_id.clone(),
                            "account.login_api".to_owned(),
                        )
                        .await?;
                }
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
    let reserved = store
        .reserved_account_aliases()
        .await?
        .into_iter()
        .collect::<HashSet<_>>();
    for row in store.account_add_receipts().await? {
        let identity: OAuthReceiptIdentity =
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
        let provider = identity.provider().to_owned();
        let alias = CredentialAlias::new(identity.alias());
        let is_import = matches!(identity, OAuthReceiptIdentity::Import(_));
        if reserved.contains(alias.as_str()) {
            continue;
        }
        match row.state.as_str() {
            "committed" => {
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
                if accounts.get(&alias).is_none() {
                    let mut descriptor = response.descriptor;
                    descriptor.active =
                        accounts.active_for_provider(&descriptor.provider).is_none();
                    accounts.add(descriptor)?;
                }
                if row.final_revision.is_none() {
                    store
                        .ensure_committed_management_revision(
                            row.command_id.clone(),
                            "account.add".to_owned(),
                        )
                        .await?;
                }
            }
            "pending" if accounts.get(&alias).is_some() && !is_import => {
                finalize_oauth_reconciled(store, accounts, &row.command_id, &alias).await?;
            }
            // A replacement import starts with a live descriptor and bundle,
            // so startup cannot distinguish "claimed, no vault write" from
            // "new bundle durable, descriptor write pending" using the
            // deliberately secret-free `{source,alias,provider}` receipt.
            // Leave it pending for same-command retry, which compares the
            // daemon-read source against the current bundle and safely
            // completes either side of that boundary.
            "pending" if accounts.get(&alias).is_some() && is_import => {}
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
                if bundle.provider_id != provider {
                    return Err(HaiderError::new(
                        ErrorCode::StoreCorrupt,
                        format!(
                            "OAuth account receipt {} provider does not match vault bundle",
                            row.command_id
                        ),
                        false,
                    ));
                }
                let active = accounts.get(&alias).map_or_else(
                    || accounts.active_for_provider(&provider).is_none(),
                    |descriptor| descriptor.active,
                );
                let descriptor = oauth_descriptor_for(&provider, &alias, &bundle, active);
                if accounts.get(&alias).is_some() {
                    accounts.replace(descriptor)?;
                } else {
                    accounts.add(descriptor)?;
                }
                finalize_oauth_reconciled(store, accounts, &row.command_id, &alias).await?;
            }
            _ => {}
        }
    }
    Ok(())
}

async fn reconcile_remove_receipts(
    store: &SqliteStoreHandle,
    accounts: &mut AccountStore<Box<dyn StoreLike>>,
    vault: &VaultProvision,
) -> Result<(), HaiderError> {
    for row in store.account_remove_receipts().await? {
        let alias = CredentialAlias::new(row.alias);
        if let Some(descriptor) = accounts.get(&alias) {
            if descriptor.provider != row.provider {
                return Err(HaiderError::new(
                    ErrorCode::StoreCorrupt,
                    format!(
                        "pending remove receipt {} reserved an alias for the wrong provider",
                        row.receipt.command_id
                    ),
                    false,
                ));
            }
            accounts.remove(&alias)?;
        }
        let VaultProvision::Available(vault) = vault else {
            continue;
        };
        let vault = Arc::clone(vault);
        let alias_for_delete = alias.clone();
        let deleted = tokio::task::spawn_blocking(move || vault.delete(&alias_for_delete)).await;
        if !matches!(deleted, Ok(Ok(()))) {
            // Keep both the pending receipt and durable reservation. Ready may
            // proceed, but add/login stays fenced and same-command remove
            // retries this idempotent deletion.
            continue;
        }
        let replacement_active_alias = accounts
            .active_for_provider(&row.provider)
            .map(|descriptor| descriptor.alias.clone());
        store
            .finalize_account_remove_receipt(
                row.receipt.command_id,
                RemoveReceipt {
                    removed_alias: alias,
                    replacement_active_alias,
                },
            )
            .await?;
    }
    Ok(())
}

async fn reconcile_set_active_receipts(
    store: &SqliteStoreHandle,
    accounts: &mut AccountStore<Box<dyn StoreLike>>,
) -> Result<(), HaiderError> {
    for row in store
        .management_receipts(ACCOUNT_SET_ACTIVE_METHOD.to_owned())
        .await?
    {
        if row.state == "committed" {
            if row.final_revision.is_none() {
                store
                    .ensure_committed_management_revision(
                        row.command_id,
                        ACCOUNT_SET_ACTIVE_METHOD.to_owned(),
                    )
                    .await?;
            }
            continue;
        }
        let identity: SetActiveIdentity =
            serde_json::from_str(&row.request_json).map_err(|error| {
                HaiderError::new(
                    ErrorCode::StoreCorrupt,
                    format!("pending set-active identity is invalid: {error}"),
                    false,
                )
            })?;
        let recovery: SetActiveRecovery = row
            .recovery_json
            .as_deref()
            .and_then(|json| serde_json::from_str(json).ok())
            .ok_or_else(|| {
                HaiderError::new(
                    ErrorCode::StoreCorrupt,
                    "pending set-active receipt has no recovery coordinates",
                    false,
                )
            })?;
        let alias = CredentialAlias::new(identity.alias);
        let Some(descriptor) = accounts.get(&alias) else {
            continue;
        };
        if descriptor.provider != recovery.provider {
            return Err(HaiderError::new(
                ErrorCode::StoreCorrupt,
                "pending set-active alias changed provider",
                false,
            ));
        }
        accounts.select(&alias)?;
        let descriptor = accounts.get(&alias).cloned().ok_or_else(|| {
            HaiderError::new(
                ErrorCode::StoreCorrupt,
                "set-active reconciliation lost the selected descriptor",
                false,
            )
        })?;
        store
            .finalize_management_receipt(
                row.command_id,
                ACCOUNT_SET_ACTIVE_METHOD.to_owned(),
                SetActiveReceipt {
                    descriptor,
                    prior_alias: recovery.prior_alias,
                },
            )
            .await?;
    }
    Ok(())
}

async fn reconcile_provider_receipts(
    store: &SqliteStoreHandle,
    providers: &mut ProviderRegistry<Box<dyn ProviderRegistryStoreLike>>,
) -> Result<(), HaiderError> {
    for method in [ACCOUNT_SET_DEFAULT_MODEL_METHOD, PROVIDER_CONFIGURE_METHOD] {
        for row in store.management_receipts(method.to_owned()).await? {
            if row.state == "committed" {
                if row.final_revision.is_none() {
                    store
                        .ensure_committed_management_revision(row.command_id, method.to_owned())
                        .await?;
                }
                continue;
            }
            let profile = if method == ACCOUNT_SET_DEFAULT_MODEL_METHOD {
                let identity: SetDefaultModelIdentity = serde_json::from_str(&row.request_json)
                    .map_err(|error| {
                        HaiderError::new(
                            ErrorCode::StoreCorrupt,
                            format!("pending default-model identity is invalid: {error}"),
                            false,
                        )
                    })?;
                providers.reconcile_set_default_model(&identity.provider, &identity.model)?
            } else {
                let input: ProviderConfigureInput = row
                    .recovery_json
                    .as_deref()
                    .and_then(|json| serde_json::from_str(json).ok())
                    .ok_or_else(|| {
                        HaiderError::new(
                            ErrorCode::StoreCorrupt,
                            "pending provider-configure receipt has no recovery coordinates",
                            false,
                        )
                    })?;
                providers.reconcile_configure(input)?
            };
            let summary = providers.summary(&profile.provider_id).ok_or_else(|| {
                HaiderError::new(
                    ErrorCode::StoreCorrupt,
                    "reconciled provider disappeared before receipt finalization",
                    false,
                )
            })?;
            store
                .finalize_management_receipt(
                    row.command_id,
                    method.to_owned(),
                    ProviderReceipt { provider: summary },
                )
                .await?;
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
        .map(|_| ())
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
        .map(|_| ())
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
        provider_names: &std::collections::BTreeSet<String>,
    ) -> Result<Self, HaiderError> {
        let descriptor_store: Box<dyn StoreLike> = match &dependencies.descriptor_store {
            Some(injected) => Box::new(DescriptorStore::Injected(Arc::clone(injected))),
            None => Box::new(DescriptorStore::Json(JsonFileStore::new(store_dir))),
        };
        let mut accounts = AccountStore::new(descriptor_store)?;
        let provider_store: Box<dyn ProviderRegistryStoreLike> =
            Box::new(JsonProviderRegistryStore::new(store_dir));
        let model_source = Arc::new(CachedProviderModelSource::default());
        let mut providers = ProviderRegistry::new(
            provider_store,
            initial_provider_profiles(provider_names, default_model),
            model_source.clone(),
        )?;
        let provider_ids = providers
            .summaries()
            .into_iter()
            .map(|summary| summary.provider)
            .collect::<Vec<_>>();
        for provider in provider_ids {
            if let Some(cached) = store.provider_models(provider.clone()).await? {
                let models = serde_json::from_str(&cached.models_json).map_err(|error| {
                    HaiderError::new(
                        ErrorCode::StoreCorrupt,
                        format!(
                            "cached model catalog for provider `{provider}` is invalid: {error}"
                        ),
                        false,
                    )
                })?;
                model_source.replace(provider.clone(), models);
            }
        }
        // R10: every daemon vault consumer funnels through this ONE wrap.
        // Global aliases are the descriptor coordinate, but the Keychain is
        // machine-global — the physical key must stay profile-scoped so two
        // profiles' identical aliases can never collide (see profile_vault.rs).
        let vault = match &dependencies.vault {
            VaultProvision::Available(inner) => VaultProvision::Available(Arc::new(
                crate::profile_vault::ProfileVault::new(Arc::clone(inner), profile_id),
            )
                as Arc<dyn Vault>),
            VaultProvision::Unsupported => VaultProvision::Unsupported,
        };
        reconcile_remove_receipts(store, &mut accounts, &vault).await?;
        reconcile_login_receipts(store, &mut accounts, &vault).await?;
        reconcile_oauth_add_receipts(store, &mut accounts, &vault).await?;
        reconcile_set_active_receipts(store, &mut accounts).await?;
        reconcile_provider_receipts(store, &mut providers).await?;
        let reserved_aliases = store
            .reserved_account_aliases()
            .await?
            .into_iter()
            .collect::<HashSet<_>>();
        let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(accounts.list().to_vec()));
        let management = ManagementSnapshot::new(
            store.management_revision().await?,
            accounts.list().to_vec(),
            providers.summaries(),
        );
        let actor_vault: Arc<dyn Vault> = match &vault {
            VaultProvision::Available(vault) => Arc::clone(vault),
            VaultProvision::Unsupported => Arc::new(MemoryVault::default()),
        };
        let refresh_fences = RefreshFenceRegistry::default();
        let actor_config = AccountActorConfig {
            store: store.clone(),
            accounts,
            vault: actor_vault,
            validator: Arc::clone(&dependencies.validator),
            snapshot: Arc::clone(&snapshot),
            management: Some(management.clone()),
            profile_id: profile_id.to_owned(),
            default_model: default_model.to_owned(),
            providers,
            provider_endpoint_validator: Arc::clone(&dependencies.provider_endpoint_validator),
            reserved_aliases,
            refresh_fences: refresh_fences.clone(),
        };
        match &vault {
            VaultProvision::Available(scoped) => {
                let oauth = OAuthCoordinator::new(
                    instance_id.to_owned(),
                    dependencies.oauth_catalog.clone(),
                    dependencies.oauth_coordinator,
                )
                .map_err(crate::oauth::oauth_error)?;
                let (actor, broker) = start_account_actor_with_broker(actor_config, |commands| {
                    CredentialBroker::new_with_fences(
                        Arc::clone(scoped),
                        dependencies.oauth_catalog.clone(),
                        Arc::clone(&snapshot),
                        commands,
                        refresh_fences,
                    )
                })?;
                let commands = actor.commands();
                Ok(Self {
                    facade: AccountsFacade {
                        login: Some(commands),
                        oauth: Some(oauth),
                        snapshot,
                        management,
                        vault_supported: true,
                    },
                    actor: Some(actor),
                    vault,
                    broker: Some(broker),
                })
            }
            VaultProvision::Unsupported => {
                let actor = start_account_actor(actor_config);
                let commands = actor.commands();
                Ok(Self {
                    facade: AccountsFacade {
                        login: Some(commands),
                        oauth: None,
                        snapshot,
                        management,
                        vault_supported: false,
                    },
                    actor: Some(actor),
                    vault: VaultProvision::Unsupported,
                    broker: None,
                })
            }
        }
    }
}

#[cfg(test)]
#[path = "accounts_tests.rs"]
mod accounts_tests;
