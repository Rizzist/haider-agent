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
use haider_core::{
    ACCOUNT_REMOVE_METHOD, ACCOUNT_SET_ACTIVE_METHOD, ACCOUNT_SET_DEFAULT_MODEL_METHOD,
    AccountAddClaim, AccountAddReceiptResponse, AccountAddReceiptRow, LoginClaim,
    LoginReceiptFailure, LoginReceiptResponse, LoginReceiptRow, ManagementClaim,
    ManagementReceiptRow, PROVIDER_CONFIGURE_METHOD, PROVIDER_REMOVE_METHOD,
    PROVIDER_SET_TRUST_METHOD,
};
use haider_core::{SqliteStoreHandle, StoreHandle};
use haider_protocol::EventPayload;
use haider_protocol::credential::{
    AccountIdentity, AuthMethod, CredentialAttentionReason, CredentialDescriptor, CredentialStatus,
    RotationCause, RotationEvent,
};
use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::ids::{CredentialAlias, DeviceId, EventId};
use haider_protocol::lockdown::{ProviderAuthChanged, ProviderTrustChanged};
use haider_provider::{
    ANTHROPIC_OAUTH_PROVIDER_NAME, ANTHROPIC_PROVIDER_NAME, AnthropicProvider,
    AnthropicTransportConfig, BEDROCK_PROVIDER_NAME, BUILTIN_PROVIDER_NAMES, CatalogError,
    CatalogSource, DEEPSEEK_BASE_URL, DEEPSEEK_PROVIDER_NAME, DiscoveredCatalog, DiscoveredModel,
    GEMINI_PROVIDER_NAME, GROK_OAUTH_PROVIDER_NAME, GeminiProvider, HAIDER_CODE_BASE_URL,
    HAIDER_CODE_PROVIDER_NAME, KIMI_OAUTH_PROVIDER_NAME, Message, OPENAI_COMPATIBLE_PROVIDER_NAME,
    OPENAI_OAUTH_PROVIDER_NAME, OPENAI_PROVIDER_NAME, OpenAiCompatibleProvider, OpenAiProvider,
    OpenAiTransportConfig, PreparedTurn, Provider, ProviderError, ProviderErrorKind,
    ProviderStream, ToolDefinition, TurnRequest, VERTEX_PROVIDER_NAME, XAI_BASE_URL,
    XAI_PROVIDER_NAME, azure_openai_origin, discover_models,
};
use haider_rpc::{
    ERROR_CODE_BUSY, ERROR_CODE_CREDENTIAL_MISSING, ERROR_CODE_INVALID_ARGUMENT,
    ERROR_CODE_PERMISSION_DENIED, ERROR_CODE_PROVIDER_ERROR, ERROR_CODE_PROVIDER_MODELS_UNKNOWN,
    ERROR_CODE_PROVIDER_REMOVE_REFUSED, ERROR_CODE_RESTAGE_REQUIRED, ERROR_CODE_REVISION_CONFLICT,
    ERROR_CODE_UNAUTHORIZED, ErrorData, ProviderApiFamilyWire, ProviderAuthRequirementWire,
    ProviderAvailabilityWire, ProviderProbeFailureWire, ProviderRemoveRefusalReasonWire,
    ProviderSummaryWire, ProviderTrustWire, RequestId, ResponseBody, StagePurpose, WireFrame,
};
use subtle::ConstantTimeEq as _;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::Instant;
use zeroize::Zeroizing;

use crate::oauth::{
    ClaudeNativeCredentialAccess, ClaudeNativeCredentialFailure, ClaudeNativeCredentialStore,
    ClaudeNativeImportError, ClaudeNativeReadEvent, CredentialBroker, KIMI_DEVICE_ALIAS,
    OAuthCoordinator, OAuthCoordinatorConfig, OAuthImportMaterial, OAuthInferenceAuthMode,
    OAuthInferenceHeaderSet, OAuthProviderCatalog, OAuthReadyClaim,
    PlatformClaudeNativeCredentialStore, RefreshFenceRegistry, is_claude_native_owner_identity,
    load_claude_native_import_material, load_oauth_import_material_with_native,
    oauth_import_source_catalog, oauth_import_source_spec, sanctioned_inference,
};
use crate::provider_registry::{
    CachedProviderModelSource, JsonProviderRegistryStore, ProductionProviderEndpointValidator,
    ProviderConfigureInput, ProviderEndpointValidator, ProviderModelSourceLike, ProviderProvenance,
    ProviderRegistry, ProviderRegistryStoreLike, ProviderTargetV1, initial_provider_profiles,
};
use crate::session_hub::FrameSink;

/// Staged-secret and pending-login-command lifetime (R7/R10: five minutes).
pub(crate) const SECRET_TTL: Duration = Duration::from_secs(300);

/// Bounded account-actor admission; overflow answers typed `busy`.
const ACTOR_CAPACITY: usize = 8;
/// Outer bound for file-backed device discovery and its already-shorter native
/// credential lookup. This is one continuous worker deadline; timing out a
/// phase never grants the next phase a fresh budget.
const DEVICE_DISCOVERY_WORKER_TIMEOUT: Duration = Duration::from_secs(5);
/// Status reuses a completed metadata-only discovery briefly. Explicit
/// account discovery remains an immediate bounded refresh.
const STATUS_DEVICE_DISCOVERY_TTL: Duration = Duration::from_secs(30);

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
    /// audited provider request), without persisting anything. `endpoint`
    /// carries the profile origin for endpoint-addressed providers (G4b:
    /// bedrock mantle, vertex); the fixed vendor endpoints ignore it.
    async fn validate(
        &self,
        provider: &str,
        model: &str,
        secret: &[u8],
        endpoint: Option<&str>,
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
        _endpoint: Option<&str>,
    ) -> Result<ValidatedIdentity, ValidationError> {
        if provider != ANTHROPIC_PROVIDER_NAME {
            return Err(ValidationError {
                kind: ValidationFailureKind::Unavailable,
                message: format!("no validator for provider {provider}"),
            });
        }
        validate_provider_api_key(provider, model, secret, None).await
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
        matches!(
            provider,
            ANTHROPIC_PROVIDER_NAME
                | OPENAI_PROVIDER_NAME
                | GEMINI_PROVIDER_NAME
                | BEDROCK_PROVIDER_NAME
                | VERTEX_PROVIDER_NAME
                | DEEPSEEK_PROVIDER_NAME
                | HAIDER_CODE_PROVIDER_NAME
                | XAI_PROVIDER_NAME
        )
    }

    async fn validate(
        &self,
        provider: &str,
        model: &str,
        secret: &[u8],
        endpoint: Option<&str>,
    ) -> Result<ValidatedIdentity, ValidationError> {
        if !self.supports(provider) {
            return Err(ValidationError {
                kind: ValidationFailureKind::Unavailable,
                message: format!("no validator for provider {provider}"),
            });
        }
        if provider == DEEPSEEK_PROVIDER_NAME {
            validate_deepseek_api_key(secret).await
        } else if provider == HAIDER_CODE_PROVIDER_NAME {
            crate::haider_code_plan::validate_api_key(secret).await
        } else if provider == XAI_PROVIDER_NAME {
            validate_xai_api_key(secret).await
        } else {
            validate_provider_api_key(provider, model, secret, endpoint).await
        }
    }
}

async fn validate_xai_api_key(secret: &[u8]) -> Result<ValidatedIdentity, ValidationError> {
    validate_openai_compatible_catalog_key(secret, CatalogSource::XaiApi, "xAI", "xai api key")
        .await
}

async fn validate_openai_compatible_catalog_key(
    secret: &[u8],
    source: CatalogSource,
    display_name: &str,
    identity: &str,
) -> Result<ValidatedIdentity, ValidationError> {
    let secret = std::str::from_utf8(secret).map_err(|_| ValidationError {
        kind: ValidationFailureKind::Unauthorized,
        message: format!("{display_name} API key is not valid UTF-8"),
    })?;
    discover_models(source, Some(secret), None)
        .await
        .map_err(|error| {
            let kind = match &error {
                CatalogError::Unavailable { reason } if reason.contains("(401)") => {
                    ValidationFailureKind::Unauthorized
                }
                CatalogError::Unauthorized => ValidationFailureKind::Unauthorized,
                CatalogError::Unavailable { reason } if reason.contains("(403)") => {
                    ValidationFailureKind::PermissionDenied
                }
                CatalogError::NotModified
                | CatalogError::Unavailable { .. }
                | CatalogError::Transport { .. }
                | CatalogError::InvalidBody { .. }
                | CatalogError::Empty => ValidationFailureKind::Unavailable,
            };
            ValidationError {
                kind,
                message: format!("{display_name} credential validation could not read /models"),
            }
        })?;
    Ok(ValidatedIdentity {
        identity: identity.to_owned(),
    })
}

/// DeepSeek key validation uses the same fixed-origin catalog source as
/// model refresh. A successful authenticated `/models` response proves the
/// key without spending tokens on a synthetic chat turn.
async fn validate_deepseek_api_key(secret: &[u8]) -> Result<ValidatedIdentity, ValidationError> {
    let secret = std::str::from_utf8(secret).map_err(|_| ValidationError {
        kind: ValidationFailureKind::Unauthorized,
        message: "DeepSeek API key is not valid UTF-8".to_owned(),
    })?;
    discover_models(CatalogSource::DeepSeekApi, Some(secret), None)
        .await
        .map_err(|error| {
            let kind = match &error {
                CatalogError::Unavailable { reason } if reason.contains("(401)") => {
                    ValidationFailureKind::Unauthorized
                }
                CatalogError::Unauthorized => ValidationFailureKind::Unauthorized,
                CatalogError::Unavailable { reason } if reason.contains("(403)") => {
                    ValidationFailureKind::PermissionDenied
                }
                CatalogError::NotModified
                | CatalogError::Unavailable { .. }
                | CatalogError::Transport { .. }
                | CatalogError::InvalidBody { .. }
                | CatalogError::Empty => ValidationFailureKind::Unavailable,
            };
            ValidationError {
                kind,
                message: "DeepSeek credential validation could not read /models".to_owned(),
            }
        })?;
    Ok(ValidatedIdentity {
        identity: "deepseek api key".to_owned(),
    })
}

async fn validate_provider_api_key(
    provider: &str,
    model: &str,
    secret: &[u8],
    endpoint: Option<&str>,
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
    let missing_endpoint = || ValidationError {
        kind: ValidationFailureKind::Unavailable,
        message: format!("provider {provider} has no configured endpoint to validate against"),
    };
    let adapter: Arc<dyn Provider> = match provider {
        ANTHROPIC_PROVIDER_NAME => {
            Arc::new(AnthropicProvider::new(handle, model).map_err(map_provider_error)?)
        }
        OPENAI_PROVIDER_NAME => {
            Arc::new(OpenAiProvider::new(handle, model).map_err(map_provider_error)?)
        }
        GEMINI_PROVIDER_NAME => {
            Arc::new(GeminiProvider::new(handle, model).map_err(map_provider_error)?)
        }
        // G4b: the enterprise anthropic surfaces validate against the
        // PROFILE endpoint through the same shape-pinned constructors the
        // turn path uses (LB2/LV1 authority — never a second URL builder).
        BEDROCK_PROVIDER_NAME => Arc::new(
            AnthropicProvider::new_endpoint(handle, model, endpoint.ok_or_else(missing_endpoint)?)
                .map_err(map_provider_error)?,
        ),
        VERTEX_PROVIDER_NAME => Arc::new(
            AnthropicProvider::new_vertex(handle, model, endpoint.ok_or_else(missing_endpoint)?)
                .map_err(map_provider_error)?,
        ),
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
        cache_metadata: None,
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

/// A custom OpenAI-compatible or Anthropic Messages profile's login target:
/// its stored origin, declared default model, and wire family. `None` for
/// fixed providers whose validator retains authority.
fn custom_login_target(
    management: Option<&ManagementSnapshot>,
    provider: &str,
) -> Option<(String, Option<String>, ProviderApiFamilyWire)> {
    if BUILTIN_PROVIDER_NAMES.contains(&provider) {
        return None;
    }
    let view = management?.read()?;
    let profile = view
        .providers
        .into_iter()
        .find(|profile| profile.provider == provider)?;
    if !matches!(
        profile.api_family,
        ProviderApiFamilyWire::OpenAiChatCompletions | ProviderApiFamilyWire::AnthropicMessages
    ) {
        return None;
    }
    // Fixed compatible builtins are excluded above. For the generic
    // compatible card, family + endpoint identifies custom rows without a
    // provenance field on the wire.
    let origin = profile.endpoint?;
    Some((origin, profile.default_model, profile.api_family))
}

/// An enterprise builtin's login target (G4b): the bedrock/vertex profile's
/// STORED endpoint plus its declared default model, so the key validates
/// against the exact origin and model spelling it will serve
/// (`anthropic.claude-fable-5` on the mantle — never the global vendor
/// default). `None` for every other provider AND for a vertex profile whose
/// card has not yet supplied an endpoint.
fn enterprise_login_target(
    management: Option<&ManagementSnapshot>,
    provider: &str,
) -> Option<(String, Option<String>)> {
    if !matches!(provider, BEDROCK_PROVIDER_NAME | VERTEX_PROVIDER_NAME) {
        return None;
    }
    let view = management?.read()?;
    let profile = view
        .providers
        .into_iter()
        .find(|profile| profile.provider == provider)?;
    let origin = profile.endpoint?;
    Some((origin, profile.default_model))
}

/// Authenticates a custom provider through its guarded model catalog. Login
/// must not require an inference side effect: a valid compatible server may
/// intentionally expose only discovery until an actual user turn arrives.
async fn validate_custom_provider_key(
    origin: &str,
    provider: &str,
    api_family: ProviderApiFamilyWire,
    secret: &[u8],
) -> Result<ValidatedIdentity, ValidationError> {
    let secret = std::str::from_utf8(secret).map_err(|_| ValidationError {
        kind: ValidationFailureKind::Unauthorized,
        message: "custom provider API key is not valid UTF-8".to_owned(),
    })?;
    let source = match api_family {
        ProviderApiFamilyWire::OpenAiChatCompletions => CatalogSource::OpenAiCompatible {
            origin: origin.to_owned(),
        },
        ProviderApiFamilyWire::AnthropicMessages => CatalogSource::AnthropicCompatible {
            origin: origin.to_owned(),
        },
        _ => {
            return Err(ValidationError {
                kind: ValidationFailureKind::Unavailable,
                message: "custom provider API family cannot validate an API key".to_owned(),
            });
        }
    };
    discover_models(source, Some(secret), None)
        .await
        .map_err(|error| ValidationError {
            kind: if matches!(error, CatalogError::Unauthorized) {
                ValidationFailureKind::Unauthorized
            } else {
                ValidationFailureKind::Unavailable
            },
            message: "custom provider credential validation could not read /v1/models".to_owned(),
        })?;
    Ok(ValidatedIdentity {
        identity: format!("{provider} api key"),
    })
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
    /// Resolve to the profile's file vault during `initialize` (it owns the
    /// store dir). The default on every platform since W5f-4.
    PlatformDefault,
    Unsupported,
}

impl VaultProvision {
    /// Platform default (W5f-4): the profile's on-disk [`FileVault`], on
    /// every platform. The macOS Keychain default was retired — every
    /// haider build is ad-hoc-signed, so the Keychain saw a fresh app each
    /// release and prompted for the login-keychain PASSWORD from a binary
    /// the user could not identify.
    #[must_use]
    pub fn platform_default() -> Self {
        Self::PlatformDefault
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
    /// G4b (LV2): the `gcloud auth print-access-token` shell-out behind the
    /// vertex gcloud device import and its auth-failure refresh. Tests
    /// inject scripted sources; production shells out.
    pub gcloud: Arc<dyn crate::gcloud::GcloudAccessTokenSource>,
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
            gcloud: Arc::new(crate::gcloud::GcloudCli),
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
#[derive(Debug)]
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

    /// Borrows a zeroizing copy for a preflight while retaining the original
    /// stage for its single consuming operation. This is intentionally
    /// narrower than `claim`: only `provider.configure` model discovery uses
    /// it before `account.login_api` consumes the same reference.
    pub(crate) fn probe(&mut self, reference: &str) -> Option<(StagePurpose, Zeroizing<Vec<u8>>)> {
        self.sweep_expired();
        let entry = self
            .entries
            .iter()
            .find(|entry| entry.reference == reference)?;
        Some((entry.purpose, Zeroizing::new(entry.secret.to_vec())))
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
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub replace_existing: bool,
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
    /// Latest published revision, for watchers. A change SIGNAL rather than a
    /// delta stream: the management view is small and already revision
    /// stamped, so a watcher re-reads `account.list` on notice instead of the
    /// daemon duplicating the snapshot onto the wire — which also sidesteps
    /// the removal-reconciliation problem that forces roster watchers to
    /// occasionally re-list.
    changes: watch::Sender<u64>,
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
            changes: watch::Sender::new(revision),
        }
    }

    /// Observe published revisions. The current value is the revision at
    /// subscription time, so a watcher that races a mutation sees it as a
    /// change rather than missing it.
    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    pub(crate) fn read(&self) -> Option<ManagementView> {
        self.inner.lock().ok().map(|view| view.clone())
    }

    pub(crate) fn inspect<T>(&self, read: impl FnOnce(&ManagementView) -> T) -> Option<T> {
        self.inner.lock().ok().map(|view| read(&view))
    }

    fn publish_accounts(&self, revision: u64, descriptors: Vec<CredentialDescriptor>) {
        if let Ok(mut view) = self.inner.lock() {
            view.revision = revision;
            view.descriptors = descriptors;
        }
        self.changes.send_replace(revision);
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
        self.changes.send_replace(revision);
    }
}

/// Lazy status-only projection of the last completed device discovery.
#[derive(Clone)]
pub(crate) struct DeviceDiscoverySnapshot {
    inner: Arc<StdMutex<DeviceDiscoveryView>>,
    disabled: bool,
}

struct DeviceDiscoveryView {
    adoption_available: Vec<haider_rpc::AccountAdoptionAvailable>,
    completed_at: Option<Instant>,
    refreshing: bool,
}

impl DeviceDiscoverySnapshot {
    pub(crate) fn new(disabled: bool) -> Self {
        Self {
            inner: Arc::new(StdMutex::new(DeviceDiscoveryView {
                adoption_available: Vec::new(),
                completed_at: None,
                refreshing: false,
            })),
            disabled,
        }
    }

    fn read_and_begin_refresh(&self) -> (Vec<haider_rpc::AccountAdoptionAvailable>, bool) {
        let Ok(mut view) = self.inner.lock() else {
            return (Vec::new(), false);
        };
        let stale = view
            .completed_at
            .is_none_or(|completed| completed.elapsed() >= STATUS_DEVICE_DISCOVERY_TTL);
        let refresh = !self.disabled && stale && !view.refreshing;
        if refresh {
            view.refreshing = true;
        }
        (view.adoption_available.clone(), refresh)
    }

    fn cancel_refresh(&self) {
        if let Ok(mut view) = self.inner.lock() {
            view.refreshing = false;
        }
    }

    fn publish(
        &self,
        result: Result<&[crate::device_discovery::DeviceCandidate], &HaiderError>,
        accounts: &[CredentialDescriptor],
    ) {
        if let Ok(mut view) = self.inner.lock() {
            if let Ok(discovered) = result {
                view.adoption_available = discovered
                    .iter()
                    .filter_map(|candidate| adoption_notice(&candidate.wire, accounts))
                    .collect();
            }
            view.completed_at = Some(Instant::now());
            view.refreshing = false;
        }
    }
}

/// The account-truth predicate the registry's seeded-inventory availability
/// rule consults (G4b, decision 6): at least one descriptor exists for the
/// provider — any status; a limited/expired account is still a credential,
/// and honesty about ITS state belongs to the account row, not the
/// provider's availability dot.
pub(crate) fn provider_has_credential<'a>(
    accounts: &'a AccountStore<Box<dyn StoreLike>>,
) -> impl Fn(&str) -> bool + 'a {
    move |provider| {
        accounts
            .list()
            .iter()
            .any(|descriptor| descriptor.provider == provider)
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
    pub discovery_disabled: bool,
    pub device_discovery: DeviceDiscoverySnapshot,
    /// The profile-scoped vault itself, for the SMALL bounded-secret
    /// surfaces the connection task serves inline (T1: the transcription
    /// secret — one ≤512-byte file read/write, comparable to one store
    /// transaction). `None` exactly when `vault_supported` is false.
    pub vault: Option<Arc<dyn Vault>>,
}

impl AccountsFacade {
    pub(crate) fn status_adoption_snapshot(&self) -> Vec<haider_rpc::AccountAdoptionAvailable> {
        let (cached, refresh) = self.device_discovery.read_and_begin_refresh();
        if refresh {
            let queued = self.login.as_ref().is_some_and(|commands| {
                commands
                    .try_send(AccountCommand::RefreshDeviceDiscovery {
                        discovery_disabled: self.discovery_disabled,
                    })
                    .is_ok()
            });
            if !queued {
                self.device_discovery.cancel_refresh();
            }
        }
        cached
    }

    pub(crate) async fn refresh_provider_models(
        &self,
        provider: String,
    ) -> Result<ProviderSummaryWire, ProviderModelsRefreshFailure> {
        let commands = self
            .login
            .as_ref()
            .ok_or_else(|| ProviderModelsRefreshFailure {
                code: ERROR_CODE_PROVIDER_MODELS_UNKNOWN.to_owned(),
                message: "provider model refresh is unavailable".to_owned(),
                retryable: true,
                data: None,
            })?;
        let (sender, receiver) = tokio::sync::oneshot::channel();
        commands
            .send(AccountCommand::RefreshProviderModels {
                provider,
                completed: ProviderModelsRefreshCompletion::internal(sender),
            })
            .await
            .map_err(|_| ProviderModelsRefreshFailure {
                code: ERROR_CODE_PROVIDER_MODELS_UNKNOWN.to_owned(),
                message: "provider model refresh actor is unavailable".to_owned(),
                retryable: true,
                data: None,
            })?;
        receiver.await.map_err(|_| ProviderModelsRefreshFailure {
            code: ERROR_CODE_PROVIDER_MODELS_UNKNOWN.to_owned(),
            message: "provider model refresh did not complete".to_owned(),
            retryable: true,
            data: None,
        })?
    }
}

/// Correlated response route back to the requesting connection. Disconnect
/// drops only this route, never the durable command.
#[derive(Clone)]
pub(crate) struct LoginRoute {
    pub request_id: RequestId,
    pub sink: Arc<dyn FrameSink>,
}

#[derive(Debug)]
pub(crate) struct ProviderModelsRefreshFailure {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub data: Option<ErrorData>,
}

pub(crate) type InternalModelRefreshSender =
    tokio::sync::oneshot::Sender<Result<ProviderSummaryWire, ProviderModelsRefreshFailure>>;

#[derive(Clone)]
pub(crate) enum ProviderModelsRefreshCompletion {
    Wire(LoginRoute),
    Internal(Arc<StdMutex<Option<InternalModelRefreshSender>>>),
}

impl ProviderModelsRefreshCompletion {
    pub(crate) fn internal(sender: InternalModelRefreshSender) -> ProviderModelsRefreshCompletion {
        Self::Internal(Arc::new(StdMutex::new(Some(sender))))
    }

    fn complete(&self, body: ResponseBody) {
        match self {
            Self::Wire(route) => respond(route, body),
            Self::Internal(sender) => {
                let result = match body {
                    ResponseBody::ProviderModelsRefresh { provider, .. } => Ok(provider),
                    ResponseBody::Error {
                        code,
                        message,
                        retryable,
                        data,
                    } => Err(ProviderModelsRefreshFailure {
                        code,
                        message,
                        retryable,
                        data,
                    }),
                    _ => Err(ProviderModelsRefreshFailure {
                        code: ERROR_CODE_PROVIDER_ERROR.to_owned(),
                        message: "provider model refresh produced a mismatched response".to_owned(),
                        retryable: false,
                        data: None,
                    }),
                };
                if let Ok(mut sender) = sender.lock()
                    && let Some(sender) = sender.take()
                {
                    let _ = sender.send(result);
                }
            }
        }
    }
}

/// One handed-off login command.
pub(crate) struct LoginJob {
    pub command_id: String,
    pub provider: String,
    pub display_alias: Option<String>,
    pub validation_model: Option<String>,
    /// Explicit key-rotation recovery intent from the wire request.
    pub replace_existing: bool,
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

pub(crate) struct DeviceImportJob {
    pub command_id: String,
    pub candidate: String,
    pub discovery_disabled: bool,
    pub route: LoginRoute,
}

pub(crate) struct SetActiveJob {
    pub command_id: String,
    pub alias: String,
    pub route: LoginRoute,
}

/// Set or clear one account's operator-chosen display label (v0.0.938).
/// Display labels are bounded: long enough to be useful, short enough that
/// no surface has to decide how to truncate someone's chosen name.
pub(crate) const ACCOUNT_LABEL_MAX_CHARS: usize = 64;

pub(crate) struct SetLabelJob {
    pub alias: String,
    /// `None` clears the label back to the provider identity.
    pub label: Option<String>,
    pub route: LoginRoute,
}

pub(crate) struct RefreshIdentityJob {
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
    /// Ephemeral model-probe credential. Excluded from every durable
    /// identity, receipt, and recovery coordinate.
    pub probe_secret: Option<Zeroizing<Vec<u8>>>,
    pub expected_revision: u64,
    pub route: LoginRoute,
}

pub(crate) struct ProviderRemoveJob {
    pub command_id: String,
    pub provider: String,
    pub expected_revision: u64,
    pub route: LoginRoute,
}

pub(crate) struct ProviderSetTrustJob {
    pub command_id: String,
    pub provider: String,
    pub trust: ProviderTrustWire,
    pub expected_revision: u64,
    pub route: LoginRoute,
}

/// Account actor mailbox items.
#[derive(Debug, Clone)]
pub(crate) struct OAuthRefreshFence {
    pub(crate) fence_epoch: u64,
    pub(crate) generation: u64,
    pub(crate) issuer: String,
    pub(crate) audience: String,
    pub(crate) resource: Option<String>,
    pub(crate) subject_hash: String,
}

#[derive(Debug, Clone)]
pub(crate) enum OAuthImportHealResult {
    NotImported,
    RefreshFallback {
        source: String,
    },
    /// A live external owner store exists. Haider may re-read it, but must
    /// never spend the independently snapshotted rotating refresh token.
    LiveOwnerStore {
        source: String,
    },
    LiveOwnerUnavailable {
        failure: ClaudeNativeCredentialFailure,
    },
    Committed {
        expected: OAuthRefreshFence,
    },
}

pub(crate) enum AccountCommand {
    Login(Box<LoginJob>),
    AddOAuth(Box<OAuthAddJob>),
    OAuthImportSources {
        completed: LoginRoute,
    },
    ImportOAuth(Box<OAuthImportJob>),
    DeviceCandidates {
        discovery_disabled: bool,
        completed: LoginRoute,
    },
    RefreshDeviceDiscovery {
        discovery_disabled: bool,
    },
    ImportDevice(Box<DeviceImportJob>),
    SetActive(Box<SetActiveJob>),
    SetLabel(Box<SetLabelJob>),
    RefreshIdentity(Box<RefreshIdentityJob>),
    Remove(Box<RemoveAccountJob>),
    SetDefaultModel(Box<SetDefaultModelJob>),
    ConfigureProvider(Box<ProviderConfigureJob>),
    RemoveProvider(Box<ProviderRemoveJob>),
    SetProviderTrust(Box<ProviderSetTrustJob>),
    RefreshProviderModels {
        provider: String,
        completed: ProviderModelsRefreshCompletion,
    },
    ProviderModelsRefreshCompleted {
        provider: String,
        cached: Option<haider_core::CachedModels>,
        result: ProviderModelsRefreshResult,
        completed: ProviderModelsRefreshCompletion,
    },
    BeginOAuthRefresh {
        descriptor: CredentialDescriptor,
        expected: OAuthRefreshFence,
        completed: tokio::sync::oneshot::Sender<Result<bool, HaiderError>>,
    },
    BeginOAuthImportHeal {
        descriptor: CredentialDescriptor,
        expected: OAuthRefreshFence,
        completed: tokio::sync::oneshot::Sender<Result<OAuthImportHealResult, HaiderError>>,
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
    pub device_discovery: DeviceDiscoverySnapshot,
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
        access_token: Option<&str>,
        etag: Option<&str>,
    ) -> Result<DiscoveredCatalog, CatalogError>;
}

struct ProductionProviderModelDiscoverer;

#[async_trait::async_trait]
impl ProviderModelDiscoverer for ProductionProviderModelDiscoverer {
    async fn discover(
        &self,
        source: CatalogSource,
        access_token: Option<&str>,
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
        Arc::new(crate::gcloud::GcloudCli),
        Arc::new(PlatformClaudeNativeCredentialStore::default()),
    )
}

fn start_account_actor_with_services(
    config: AccountActorConfig,
    build_broker: impl FnOnce(mpsc::Sender<AccountCommand>) -> Result<CredentialBroker, HaiderError>,
    model_discoverer: Arc<dyn ProviderModelDiscoverer>,
    gcloud: Arc<dyn crate::gcloud::GcloudAccessTokenSource>,
    claude_native: Arc<dyn ClaudeNativeCredentialStore>,
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
        gcloud,
        claude_native,
    );
    Ok((handle, broker))
}

#[allow(clippy::too_many_arguments)]
fn spawn_account_actor(
    config: AccountActorConfig,
    commands: mpsc::Sender<AccountCommand>,
    receiver: mpsc::Receiver<AccountCommand>,
    force_stop: watch::Sender<bool>,
    forced: watch::Receiver<bool>,
    broker: Option<CredentialBroker>,
    model_discoverer: Arc<dyn ProviderModelDiscoverer>,
    gcloud: Arc<dyn crate::gcloud::GcloudAccessTokenSource>,
    claude_native: Arc<dyn ClaudeNativeCredentialStore>,
) -> AccountActorHandle {
    let claude_native: Arc<dyn ClaudeNativeCredentialStore> =
        Arc::new(ClaudeNativeCredentialAccess::new(claude_native));
    let task = tokio::spawn(run_account_actor(
        config,
        commands.clone(),
        receiver,
        forced,
        broker,
        model_discoverer,
        gcloud,
        claude_native,
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

#[allow(clippy::too_many_arguments)]
async fn run_account_actor(
    config: AccountActorConfig,
    commands: mpsc::Sender<AccountCommand>,
    mut receiver: mpsc::Receiver<AccountCommand>,
    mut force_stop: watch::Receiver<bool>,
    broker: Option<CredentialBroker>,
    model_discoverer: Arc<dyn ProviderModelDiscoverer>,
    gcloud: Arc<dyn crate::gcloud::GcloudAccessTokenSource>,
    claude_native: Arc<dyn ClaudeNativeCredentialStore>,
) {
    let AccountActorConfig {
        store,
        mut accounts,
        vault,
        validator,
        snapshot,
        management,
        device_discovery,
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
    let mut device_discovery_refreshes: JoinSet<
        Result<Vec<crate::device_discovery::DeviceCandidate>, HaiderError>,
    > = JoinSet::new();
    let mut model_refresh_routes = HashMap::new();
    let mut refreshing_providers = HashSet::new();
    let mut draining = false;
    if backfill_oauth_identities(&mut accounts, vault.as_ref()).await {
        refresh_resolver_snapshot(&snapshot, &accounts);
        let _ = publish_next_management_revision(&store, &snapshot, management.as_ref(), &accounts)
            .await;
    }
    loop {
        if draining
            && model_refreshes.is_empty()
            && refreshing_providers.is_empty()
            && device_discovery_refreshes.is_empty()
        {
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
                                respond_model_refresh_error(
                                    &route,
                                    ERROR_CODE_PROVIDER_ERROR,
                                    "provider model refresh worker failed",
                                    true,
                                    None,
                                );
                            }
                        }
                    }
                }
                continue;
            }
            completed = device_discovery_refreshes.join_next(), if !device_discovery_refreshes.is_empty() => {
                match completed {
                    Some(Ok(result)) => {
                        device_discovery.publish(result.as_deref(), accounts.list());
                    }
                    Some(Err(_)) => {
                        let error = device_discovery_worker_error("background worker was lost");
                        device_discovery.publish(Err(&error), accounts.list());
                    }
                    None => {}
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
                    &providers,
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
            AccountCommand::OAuthImportSources { completed } => {
                respond(
                    &completed,
                    ResponseBody::AccountOAuthImportSources {
                        sources: oauth_import_source_catalog(claude_native.as_ref()),
                    },
                );
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
                    None,
                    OAuthCommitResponse::ImportLegacy,
                    None,
                    None,
                    ClaudeNativeReadEvent::Significant,
                    Arc::clone(&claude_native),
                )
                .await;
            }
            AccountCommand::DeviceCandidates {
                discovery_disabled,
                completed,
            } => {
                let discovered = discover_candidates(
                    Arc::clone(&claude_native),
                    discovery_disabled,
                    ClaudeNativeReadEvent::AdoptionDiscovery,
                )
                .await;
                match discovered {
                    Ok(discovered) => {
                        device_discovery.publish(Ok(&discovered), accounts.list());
                        let adoption_available = discovered
                            .iter()
                            .filter_map(|candidate| {
                                adoption_notice(&candidate.wire, accounts.list())
                            })
                            .collect();
                        let candidates = discovered
                            .into_iter()
                            .map(|candidate| candidate.wire)
                            .collect();
                        respond(
                            &completed,
                            ResponseBody::AccountDeviceCandidates {
                                discovery_disabled: crate::device_discovery::discovery_is_disabled(
                                    discovery_disabled,
                                ),
                                candidates,
                                adoption_available,
                            },
                        );
                    }
                    Err(error) => {
                        device_discovery.publish(Err(&error), accounts.list());
                        respond_management_error(&completed, &error);
                    }
                }
            }
            AccountCommand::RefreshDeviceDiscovery { discovery_disabled } => {
                if device_discovery_refreshes.is_empty() {
                    let claude_native = Arc::clone(&claude_native);
                    device_discovery_refreshes.spawn(async move {
                        discover_candidates(
                            claude_native,
                            discovery_disabled,
                            ClaudeNativeReadEvent::AdoptionDiscovery,
                        )
                        .await
                    });
                }
            }
            AccountCommand::ImportDevice(job) => {
                handle_device_import(
                    &store,
                    &mut accounts,
                    Arc::clone(&vault),
                    &snapshot,
                    management.as_ref(),
                    &providers,
                    &reserved_aliases,
                    &refresh_fences,
                    Arc::clone(&gcloud),
                    Arc::clone(&claude_native),
                    *job,
                )
                .await;
            }
            AccountCommand::SetLabel(job) => {
                handle_set_label(&store, &mut accounts, &snapshot, management.as_ref(), *job).await;
            }
            AccountCommand::RefreshIdentity(job) => {
                handle_refresh_identity(
                    &store,
                    &mut accounts,
                    vault.as_ref(),
                    &snapshot,
                    management.as_ref(),
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
                    ProviderConfigureContext {
                        store: &store,
                        accounts: &accounts,
                        vault: vault.as_ref(),
                        management: management.as_ref(),
                        providers: &mut providers,
                        endpoint_validator: Arc::clone(&provider_endpoint_validator),
                        model_discoverer: model_discoverer.as_ref(),
                    },
                    *job,
                )
                .await;
            }
            AccountCommand::RemoveProvider(job) => {
                let refresh_in_progress = refreshing_providers.contains(&job.provider);
                handle_provider_remove(
                    &store,
                    &accounts,
                    management.as_ref(),
                    &mut providers,
                    refresh_in_progress,
                    *job,
                )
                .await;
            }
            AccountCommand::SetProviderTrust(job) => {
                handle_provider_set_trust(
                    &store,
                    &accounts,
                    management.as_ref(),
                    &mut providers,
                    *job,
                )
                .await;
            }
            AccountCommand::RefreshProviderModels {
                provider,
                completed,
            } => {
                if draining {
                    respond_model_refresh_error(
                        &completed,
                        ERROR_CODE_BUSY,
                        "account actor is shutting down",
                        true,
                        None,
                    );
                    continue;
                }
                begin_provider_models_refresh(
                    &store,
                    &accounts,
                    &providers,
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
                    ProviderModelsRefreshContext {
                        store: &store,
                        accounts: &accounts,
                        management: management.as_ref(),
                        providers: &providers,
                        completed: &completed,
                    },
                    provider,
                    cached,
                    result,
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
            AccountCommand::BeginOAuthImportHeal {
                descriptor,
                expected,
                completed,
            } => {
                let result = handle_oauth_import_heal(
                    &store,
                    &mut accounts,
                    Arc::clone(&vault),
                    &snapshot,
                    management.as_ref(),
                    &reserved_aliases,
                    &refresh_fences,
                    &descriptor,
                    &expected,
                    Arc::clone(&claude_native),
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

async fn backfill_oauth_identities(
    accounts: &mut AccountStore<Box<dyn StoreLike>>,
    vault: &dyn Vault,
) -> bool {
    let targets = accounts
        .list()
        .iter()
        .filter(|descriptor| {
            descriptor.auth_method == AuthMethod::OAuth && descriptor.account_identity.is_none()
        })
        .map(|descriptor| (descriptor.alias.clone(), descriptor.provider.clone()))
        .collect::<Vec<_>>();
    let mut changed = false;
    for (alias, provider) in targets {
        let Ok(stored) = vault.resolve(&alias) else {
            continue;
        };
        let Ok(bundle) = haider_accounts::OAuthTokenBundleV1::decode(stored.expose_secret()) else {
            continue;
        };
        let identity = bundle.account_identity.clone().or_else(|| {
            let source = haider_provider::oauth_identity_source(&provider)?;
            source
                .identity_from_tokens(&haider_provider::OAuthTokens {
                    access_token: bundle.access_token(),
                    refresh_token: bundle.refresh_token(),
                    id_token: bundle.id_token(),
                    captured_at: unix_ms_after(Duration::ZERO),
                })
                .ok()
                .flatten()
        });
        if let Some(identity) = identity
            && accounts
                .backfill_identity(&alias, identity)
                .unwrap_or(false)
        {
            changed = true;
        }
    }
    changed
}

fn adoption_notice(
    candidate: &haider_rpc::DeviceCredentialCandidateWire,
    accounts: &[CredentialDescriptor],
) -> Option<haider_rpc::AccountAdoptionAvailable> {
    if !candidate.import_supported || !matches!(candidate.source.as_str(), "codex" | "claude-code")
    {
        return None;
    }
    let identity = candidate.identity.as_ref();
    let email = identity.and_then(|identity| identity.email.clone());
    let already_present = accounts.iter().any(|descriptor| {
        descriptor.provider == candidate.provider
            && descriptor
                .account_identity
                .as_ref()
                .zip(identity)
                .is_some_and(|(existing, candidate)| account_identities_match(candidate, existing))
    });
    (!already_present).then(|| haider_rpc::AccountAdoptionAvailable {
        source: candidate.source.clone(),
        email,
    })
}

fn account_identities_match(candidate: &AccountIdentity, existing: &AccountIdentity) -> bool {
    if candidate
        .account_id
        .as_deref()
        .zip(existing.account_id.as_deref())
        .is_some_and(|(candidate, existing)| candidate == existing)
        || candidate
            .email
            .as_deref()
            .zip(existing.email.as_deref())
            .is_some_and(|(candidate, existing)| candidate.eq_ignore_ascii_case(existing))
    {
        return true;
    }
    // Claude Code currently exposes only a provider-normalized subscription
    // identity. Match that complete available tuple, but never let it
    // override a disagreeing stable email/account id.
    if candidate.account_id.is_some()
        || candidate.email.is_some()
        || existing.account_id.is_some()
        || existing.email.is_some()
    {
        return false;
    }
    (candidate.display_name.is_some() || candidate.plan.is_some())
        && candidate.display_name == existing.display_name
        && candidate.plan == existing.plan
        && candidate.issuer == existing.issuer
}

#[allow(clippy::too_many_arguments)]
async fn begin_provider_models_refresh(
    store: &SqliteStoreHandle,
    accounts: &AccountStore<Box<dyn StoreLike>>,
    providers: &ProviderRegistry<Box<dyn ProviderRegistryStoreLike>>,
    broker: Option<&CredentialBroker>,
    model_discoverer: &Arc<dyn ProviderModelDiscoverer>,
    commands: &mpsc::Sender<AccountCommand>,
    refresh_tasks: &mut JoinSet<()>,
    refresh_routes: &mut HashMap<tokio::task::Id, (String, ProviderModelsRefreshCompletion)>,
    refreshing_providers: &mut HashSet<String>,
    provider: String,
    completed: ProviderModelsRefreshCompletion,
) {
    let Some((source, auth_requirement)) = catalog_source(&provider, providers) else {
        respond_provider_models_unavailable(
            &completed,
            &provider,
            "provider does not expose a subscription model catalog",
        );
        return;
    };
    if refreshing_providers.contains(&provider) {
        respond_model_refresh_error(
            &completed,
            ERROR_CODE_BUSY,
            "a model refresh is already running for this provider",
            true,
            None,
        );
        return;
    }
    let expected_auth = match auth_requirement {
        ProviderAuthRequirementWire::OAuth => Some(AuthMethod::OAuth),
        ProviderAuthRequirementWire::ApiKey => Some(AuthMethod::ApiKey),
        ProviderAuthRequirementWire::None => None,
        ProviderAuthRequirementWire::Unknown => {
            respond_provider_models_unavailable(
                &completed,
                &provider,
                "provider model discovery has an unsupported authentication requirement",
            );
            return;
        }
        _ => {
            respond_provider_models_unavailable(
                &completed,
                &provider,
                "provider model discovery has an unsupported authentication requirement",
            );
            return;
        }
    };
    let descriptor = if let Some(expected_auth) = expected_auth {
        let Some(descriptor) = accounts.active_for_provider(&provider).cloned() else {
            respond_model_refresh_error(
                &completed,
                ERROR_CODE_CREDENTIAL_MISSING,
                "provider has no active credential",
                false,
                None,
            );
            return;
        };
        if descriptor.auth_method != expected_auth {
            let reason = if expected_auth == AuthMethod::OAuth {
                "provider model discovery requires an active OAuth credential"
            } else {
                "provider model discovery requires an active API-key credential"
            };
            respond_provider_models_unavailable(&completed, &provider, reason);
            return;
        }
        Some(descriptor)
    } else {
        None
    };
    let broker = if descriptor.is_some() {
        let Some(broker) = broker.cloned() else {
            let message = if expected_auth == Some(AuthMethod::OAuth) {
                "OAuth credential broker is unavailable"
            } else {
                "credential broker is unavailable"
            };
            respond_model_refresh_error(
                &completed,
                ERROR_CODE_CREDENTIAL_MISSING,
                message,
                true,
                None,
            );
            return;
        };
        Some(broker)
    } else {
        None
    };
    let cached = match store.provider_models(provider.clone()).await {
        Ok(cached) => cached,
        Err(error) => {
            respond_model_refresh_error(
                &completed,
                ERROR_CODE_PROVIDER_ERROR,
                &error.message,
                error.retryable,
                None,
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
        let result = match (broker, descriptor) {
            (Some(broker), Some(descriptor)) => match broker.resolve(&descriptor).await {
                Ok(access_token) => {
                    let access_token =
                        std::str::from_utf8(access_token.expose_secret()).map_err(|_| {
                            let message = if descriptor.auth_method == AuthMethod::OAuth {
                                "OAuth access token is not valid UTF-8"
                            } else {
                                "API key is not valid UTF-8"
                            };
                            HaiderError::new(ErrorCode::CredentialMissing, message, false)
                        });
                    match access_token {
                        Ok(access_token) => ProviderModelsRefreshResult::Discovery(
                            model_discoverer
                                .discover(source, Some(access_token), etag.as_deref())
                                .await,
                        ),
                        Err(error) => ProviderModelsRefreshResult::Credential(error),
                    }
                }
                Err(error) => ProviderModelsRefreshResult::Credential(error),
            },
            (None, None) => ProviderModelsRefreshResult::Discovery(
                model_discoverer
                    .discover(source, None, etag.as_deref())
                    .await,
            ),
            _ => ProviderModelsRefreshResult::Credential(HaiderError::new(
                ErrorCode::Internal,
                "model discovery credential state is inconsistent",
                false,
            )),
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

struct ProviderModelsRefreshContext<'a> {
    store: &'a SqliteStoreHandle,
    accounts: &'a AccountStore<Box<dyn StoreLike>>,
    management: Option<&'a ManagementSnapshot>,
    providers: &'a ProviderRegistry<Box<dyn ProviderRegistryStoreLike>>,
    completed: &'a ProviderModelsRefreshCompletion,
}

async fn finish_provider_models_refresh(
    context: ProviderModelsRefreshContext<'_>,
    provider: String,
    cached: Option<haider_core::CachedModels>,
    result: ProviderModelsRefreshResult,
) {
    let ProviderModelsRefreshContext {
        store,
        accounts,
        management,
        providers,
        completed,
    } = context;
    match result {
        ProviderModelsRefreshResult::Discovery(Ok(catalog)) => {
            let models_json = match serde_json::to_string(&catalog.models) {
                Ok(models_json) => models_json,
                Err(error) => {
                    respond_model_refresh_error(
                        completed,
                        ERROR_CODE_PROVIDER_ERROR,
                        &format!("could not encode provider model catalog: {error}"),
                        false,
                        None,
                    );
                    return;
                }
            };
            let fetched_at_ms = unix_ms_after(Duration::ZERO);
            let revision = match store
                .put_provider_models_and_advance_management_revision(
                    provider.clone(),
                    models_json,
                    catalog.etag,
                    fetched_at_ms,
                )
                .await
            {
                Ok(revision) => revision,
                Err(error) => {
                    respond_model_refresh_error(
                        completed,
                        ERROR_CODE_PROVIDER_ERROR,
                        &error.message,
                        error.retryable,
                        None,
                    );
                    return;
                }
            };
            providers.replace_models(provider.clone(), catalog.models, Some(fetched_at_ms));
            let summaries = providers.summaries(&provider_has_credential(accounts));
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
            completed.complete(ResponseBody::ProviderModelsRefresh {
                provider: summary,
                revision,
            });
        }
        ProviderModelsRefreshResult::Discovery(Err(CatalogError::NotModified)) => {
            let Some(cached) = cached else {
                respond_model_refresh_error(
                    completed,
                    ERROR_CODE_PROVIDER_ERROR,
                    "provider returned not-modified without a cached catalog",
                    true,
                    None,
                );
                return;
            };
            let fetched_at_ms = unix_ms_after(Duration::ZERO);
            if let Err(error) = store
                .put_provider_models(
                    provider.clone(),
                    cached.models_json,
                    cached.etag,
                    fetched_at_ms,
                )
                .await
            {
                respond_model_refresh_error(
                    completed,
                    ERROR_CODE_PROVIDER_ERROR,
                    &error.message,
                    error.retryable,
                    None,
                );
                return;
            }
            providers.touch_models(&provider, fetched_at_ms);
            let revision = match store.management_revision().await {
                Ok(revision) => revision,
                Err(error) => {
                    respond_model_refresh_error(
                        completed,
                        ERROR_CODE_PROVIDER_ERROR,
                        &error.message,
                        error.retryable,
                        None,
                    );
                    return;
                }
            };
            let summaries = providers.summaries(&provider_has_credential(accounts));
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
            completed.complete(ResponseBody::ProviderModelsRefresh {
                provider: summary,
                revision,
            });
        }
        ProviderModelsRefreshResult::Discovery(Err(CatalogError::Unavailable { reason })) => {
            if let Some(data) =
                custom_probe_error_data(providers, &provider, ProviderProbeFailureWire::Unavailable)
            {
                respond_model_refresh_error(
                    completed,
                    ERROR_CODE_PROVIDER_ERROR,
                    &reason,
                    false,
                    Some(data),
                );
            } else {
                respond_provider_models_unavailable(completed, &provider, &reason);
            }
        }
        ProviderModelsRefreshResult::Discovery(Err(CatalogError::Transport { reason })) => {
            respond_model_refresh_error(
                completed,
                ERROR_CODE_PROVIDER_ERROR,
                &reason,
                true,
                custom_probe_error_data(
                    providers,
                    &provider,
                    ProviderProbeFailureWire::Unreachable,
                ),
            );
        }
        ProviderModelsRefreshResult::Discovery(Err(CatalogError::Unauthorized)) => {
            respond_model_refresh_error(
                completed,
                ERROR_CODE_UNAUTHORIZED,
                "model catalog authentication failed",
                false,
                custom_probe_error_data(
                    providers,
                    &provider,
                    ProviderProbeFailureWire::Unauthorized,
                ),
            );
        }
        ProviderModelsRefreshResult::Discovery(Err(CatalogError::InvalidBody { reason })) => {
            if let Some(data) = custom_probe_error_data(
                providers,
                &provider,
                ProviderProbeFailureWire::NonOpenAiCompatibleBody,
            ) {
                respond_model_refresh_error(
                    completed,
                    ERROR_CODE_PROVIDER_ERROR,
                    &reason,
                    false,
                    Some(data),
                );
            } else {
                respond_provider_models_unavailable(completed, &provider, &reason);
            }
        }
        ProviderModelsRefreshResult::Discovery(Err(CatalogError::Empty)) => {
            let reason = "provider returned an empty model list";
            if let Some(data) =
                custom_probe_error_data(providers, &provider, ProviderProbeFailureWire::EmptyList)
            {
                respond_model_refresh_error(
                    completed,
                    ERROR_CODE_PROVIDER_ERROR,
                    reason,
                    false,
                    Some(data),
                );
            } else {
                respond_provider_models_unavailable(completed, &provider, reason);
            }
        }
        ProviderModelsRefreshResult::Credential(error) => {
            respond_model_refresh_error(
                completed,
                ERROR_CODE_PROVIDER_ERROR,
                &error.message,
                error.retryable,
                None,
            );
        }
    }
}

fn custom_probe_error_data(
    providers: &ProviderRegistry<Box<dyn ProviderRegistryStoreLike>>,
    provider: &str,
    failure: ProviderProbeFailureWire,
) -> Option<ErrorData> {
    providers
        .get(provider)
        .is_some_and(|profile| matches!(profile.provenance, ProviderProvenance::Custom))
        .then(|| ErrorData::ProviderProbeFailed {
            provider: provider.to_owned(),
            failure,
        })
}

fn catalog_source(
    provider: &str,
    providers: &ProviderRegistry<Box<dyn ProviderRegistryStoreLike>>,
) -> Option<(CatalogSource, ProviderAuthRequirementWire)> {
    match provider {
        OPENAI_OAUTH_PROVIDER_NAME => Some((
            CatalogSource::OpenAiSubscription,
            ProviderAuthRequirementWire::OAuth,
        )),
        ANTHROPIC_OAUTH_PROVIDER_NAME => Some((
            CatalogSource::AnthropicSubscription,
            ProviderAuthRequirementWire::OAuth,
        )),
        KIMI_OAUTH_PROVIDER_NAME => {
            Some((CatalogSource::KimiOAuth, ProviderAuthRequirementWire::OAuth))
        }
        GROK_OAUTH_PROVIDER_NAME => {
            Some((CatalogSource::GrokOAuth, ProviderAuthRequirementWire::OAuth))
        }
        DEEPSEEK_PROVIDER_NAME => Some((
            CatalogSource::DeepSeekApi,
            ProviderAuthRequirementWire::ApiKey,
        )),
        HAIDER_CODE_PROVIDER_NAME => Some((
            CatalogSource::HaiderCodeApi,
            ProviderAuthRequirementWire::ApiKey,
        )),
        XAI_PROVIDER_NAME => Some((CatalogSource::XaiApi, ProviderAuthRequirementWire::ApiKey)),
        GEMINI_PROVIDER_NAME => Some((
            CatalogSource::GeminiApiKey,
            ProviderAuthRequirementWire::ApiKey,
        )),
        _ => {
            let profile = providers.get(provider)?;
            if profile.provenance != ProviderProvenance::Custom
                || !matches!(
                    profile.api_family,
                    ProviderApiFamilyWire::OpenAiChatCompletions
                        | ProviderApiFamilyWire::AnthropicMessages
                )
                || !matches!(
                    profile.auth_requirement,
                    ProviderAuthRequirementWire::ApiKey | ProviderAuthRequirementWire::None
                )
            {
                return None;
            }
            let origin = profile.base_url.clone()?;
            let source = match profile.api_family {
                ProviderApiFamilyWire::OpenAiChatCompletions => {
                    CatalogSource::OpenAiCompatible { origin }
                }
                ProviderApiFamilyWire::AnthropicMessages => {
                    CatalogSource::AnthropicCompatible { origin }
                }
                _ => return None,
            };
            Some((source, profile.auth_requirement))
        }
    }
}

fn respond_provider_models_unavailable(
    completed: &ProviderModelsRefreshCompletion,
    provider: &str,
    reason: &str,
) {
    respond_model_refresh_error(
        completed,
        ERROR_CODE_PROVIDER_ERROR,
        reason,
        false,
        Some(ErrorData::ProviderModelsUnavailable {
            provider: provider.to_owned(),
            reason: reason.to_owned(),
        }),
    );
}

fn respond_model_refresh_error(
    completed: &ProviderModelsRefreshCompletion,
    code: &str,
    message: &str,
    retryable: bool,
    data: Option<ErrorData>,
) {
    completed.complete(ResponseBody::Error {
        code: code.to_owned(),
        message: message.to_owned(),
        retryable,
        data,
    });
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
                    CredentialStatus::Expired
                    | CredentialStatus::Revoked
                    | CredentialStatus::NeedsAttention { .. } => false,
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
            CredentialStatus::Expired | CredentialStatus::NeedsAttention { .. } => {
                Some(RotationTrigger::AuthExpired)
            }
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
    if accounts.get(&alias).is_some_and(|current| {
        matches!(
            &current.status,
            CredentialStatus::Expired | CredentialStatus::NeedsAttention { .. }
        )
    }) && !matches!(
        &descriptor.status,
        CredentialStatus::Expired | CredentialStatus::NeedsAttention { .. }
    ) {
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

/// The display-label mutation. Deliberately NOT receipted, unlike its
/// sibling account doors: a label carries no credential authority and is
/// idempotent BY VALUE — replaying the same request produces the same
/// descriptor — so a command receipt would add machinery without adding
/// safety. Everything that changes what a turn spends (set_active, login,
/// remove) stays receipted.
///
/// The write goes through `AccountStore::replace`, which holds the alias,
/// provider, auth method and base URL immutable, so a rename can only ever
/// change the cosmetic field. Publishing bumps the management revision,
/// which is what notifies `account.list_watch` subscribers.
async fn handle_set_label(
    store: &SqliteStoreHandle,
    accounts: &mut AccountStore<Box<dyn StoreLike>>,
    snapshot: &AccountsSnapshot,
    management: Option<&ManagementSnapshot>,
    job: SetLabelJob,
) {
    let alias = CredentialAlias::new(job.alias.trim());
    let Some(existing) = accounts
        .list()
        .iter()
        .find(|descriptor| descriptor.alias == alias)
        .cloned()
    else {
        respond_management_error(
            &job.route,
            &HaiderError::new(
                ErrorCode::CredentialMissing,
                format!("no credential named `{alias}`"),
                false,
            ),
        );
        return;
    };
    // Bounded, control-stripped, and empty-means-clear: a label is display
    // text and must never smuggle escapes into a terminal surface.
    let label = job.label.and_then(|label| {
        let cleaned: String = label
            .chars()
            .filter(|c| !c.is_control())
            .take(ACCOUNT_LABEL_MAX_CHARS)
            .collect();
        let trimmed = cleaned.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    });

    drop(existing);
    if let Err(error) = accounts.set_label(&alias, label) {
        respond_management_error(&job.route, &error);
        return;
    }
    let revision =
        match publish_next_management_revision(store, snapshot, management, accounts).await {
            Ok(revision) => revision,
            Err(error) => {
                respond_management_error(&job.route, &error);
                return;
            }
        };
    let Some(descriptor) = accounts
        .list()
        .iter()
        .find(|descriptor| descriptor.alias == alias)
        .cloned()
    else {
        respond_management_error(
            &job.route,
            &HaiderError::new(
                ErrorCode::StoreCorrupt,
                "credential disappeared while setting its label",
                false,
            ),
        );
        return;
    };
    respond(
        &job.route,
        ResponseBody::AccountSetLabel {
            descriptor,
            revision,
        },
    );
}

/// Rebuilds public identity from the credential already inside Haider's
/// vault. OAuth providers all pass through the provider-owned abstraction;
/// no token bytes enter the response, receipt, or diagnostic path.
async fn handle_refresh_identity(
    store: &SqliteStoreHandle,
    accounts: &mut AccountStore<Box<dyn StoreLike>>,
    vault: &dyn Vault,
    snapshot: &AccountsSnapshot,
    management: Option<&ManagementSnapshot>,
    job: RefreshIdentityJob,
) {
    let alias = CredentialAlias::new(job.alias.trim());
    let Some(current) = accounts
        .list()
        .iter()
        .find(|descriptor| descriptor.alias == alias)
        .cloned()
    else {
        respond_management_error(
            &job.route,
            &HaiderError::new(
                ErrorCode::CredentialMissing,
                format!("no credential named `{alias}`"),
                false,
            ),
        );
        return;
    };
    let stored = match vault.resolve(&alias) {
        Ok(stored) => stored,
        Err(error) => {
            respond_management_error(&job.route, &error);
            return;
        }
    };
    let identity = match current.auth_method {
        AuthMethod::ApiKey => Some(api_key_identity(&current.provider, stored.expose_secret())),
        AuthMethod::OAuth => {
            let bundle = match haider_accounts::OAuthTokenBundleV1::decode(stored.expose_secret()) {
                Ok(bundle) => bundle,
                Err(error) => {
                    respond_management_error(&job.route, &error);
                    return;
                }
            };
            let Some(source) = haider_provider::oauth_identity_source(&current.provider) else {
                respond_management_error(
                    &job.route,
                    &HaiderError::new(
                        ErrorCode::ProviderError,
                        format!(
                            "provider `{}` has no OAuth identity adapter",
                            current.provider
                        ),
                        false,
                    ),
                );
                return;
            };
            match source.identity_from_tokens(&haider_provider::OAuthTokens {
                access_token: bundle.access_token(),
                refresh_token: bundle.refresh_token(),
                id_token: bundle.id_token(),
                captured_at: unix_ms_after(Duration::ZERO),
            }) {
                Ok(Some(identity)) => Some(identity),
                Ok(None) => bundle
                    .account_identity
                    .clone()
                    .or(current.account_identity.clone()),
                Err(_) => {
                    respond_management_error(
                        &job.route,
                        &HaiderError::new(
                            ErrorCode::ProviderError,
                            format!(
                                "stored OAuth identity for provider `{}` is malformed",
                                current.provider
                            ),
                            false,
                        ),
                    );
                    return;
                }
            }
        }
    };
    let descriptor = match accounts.set_identity(&alias, identity) {
        Ok(descriptor) => descriptor,
        Err(error) => {
            respond_management_error(&job.route, &error);
            return;
        }
    };
    let revision =
        match publish_next_management_revision(store, snapshot, management, accounts).await {
            Ok(revision) => revision,
            Err(error) => {
                respond_management_error(&job.route, &error);
                return;
            }
        };
    respond(
        &job.route,
        ResponseBody::AccountRefresh {
            descriptor,
            revision,
        },
    );
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
struct ProviderConfigureRecovery {
    #[serde(flatten)]
    input: ProviderConfigureInput,
    /// The prior custom-provider auth mode, retained so crash reconciliation
    /// journals the same transition even if the profile file was already saved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    previous_auth_requirement: Option<ProviderAuthRequirementWire>,
    /// Public catalog facts already authenticated before the receipt claim.
    /// Retaining them lets a crash-resume rebuild the durable cache without
    /// persisting or reusing the ephemeral probe credential.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    discovered_models: Option<Vec<DiscoveredModel>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    discovered_etag: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    revision_unchanged: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    revision_unchanged_response: Option<ProviderReceipt>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct ProviderRemoveIdentity {
    provider: String,
    expected_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct ProviderSetTrustIdentity {
    provider: String,
    trust: ProviderTrustWire,
    expected_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct ProviderSetTrustRecovery {
    previous: ProviderTrustWire,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct ProviderReceipt {
    provider: ProviderSummaryWire,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    revision_unchanged: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct ProviderRemoveReceipt {
    provider: String,
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

fn respond_provider_remove_refused(
    route: &LoginRoute,
    provider: &str,
    reason: ProviderRemoveRefusalReasonWire,
    blocking_aliases: Vec<String>,
) {
    let message = match reason {
        ProviderRemoveRefusalReasonWire::NotFound => {
            format!("provider `{provider}` is not registered")
        }
        ProviderRemoveRefusalReasonWire::ReleaseOwned => {
            format!("provider `{provider}` is release-owned and cannot be removed")
        }
        ProviderRemoveRefusalReasonWire::BlockingAccounts => format!(
            "provider `{provider}` is referenced by credential aliases: {}",
            blocking_aliases.join(", ")
        ),
        _ => format!("provider `{provider}` cannot be removed"),
    };
    respond(
        route,
        ResponseBody::Error {
            code: ERROR_CODE_PROVIDER_REMOVE_REFUSED.to_owned(),
            message,
            retryable: false,
            data: Some(ErrorData::ProviderRemoveRefused {
                provider: provider.to_owned(),
                reason,
                blocking_aliases,
            }),
        },
    );
}

fn provider_blocking_aliases(
    accounts: &AccountStore<Box<dyn StoreLike>>,
    provider: &str,
) -> Vec<String> {
    let mut aliases = accounts
        .list()
        .iter()
        .filter(|descriptor| descriptor.provider == provider)
        .map(|descriptor| descriptor.alias.as_str().to_owned())
        .collect::<Vec<_>>();
    aliases.sort();
    aliases
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
        management.publish(
            revision,
            accounts.list().to_vec(),
            providers.summaries(&provider_has_credential(accounts)),
        );
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
    // management snapshot waits for the receipt/revision transaction. If the
    // daemon dies after this projection change but before the durable delete,
    // the already-durable pending receipt + alias reservation makes startup
    // repeat both removals. We finalize the durable tombstone only after the
    // vault delete so an acknowledged remove can never retain live material.
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
        management.publish(
            revision,
            accounts.list().to_vec(),
            providers.summaries(&provider_has_credential(accounts)),
        );
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
    let Some(provider) =
        providers.summary(&profile.provider_id, &provider_has_credential(accounts))
    else {
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
    let receipt = ProviderReceipt {
        provider,
        revision_unchanged: false,
    };
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
        management.publish(
            revision,
            accounts.list().to_vec(),
            providers.summaries(&provider_has_credential(accounts)),
        );
    }
    respond(
        &job.route,
        ResponseBody::AccountSetDefaultModel {
            provider: receipt.provider,
            revision,
        },
    );
}

struct ProviderConfigureContext<'a> {
    store: &'a SqliteStoreHandle,
    accounts: &'a AccountStore<Box<dyn StoreLike>>,
    vault: &'a dyn Vault,
    management: Option<&'a ManagementSnapshot>,
    providers: &'a mut ProviderRegistry<Box<dyn ProviderRegistryStoreLike>>,
    endpoint_validator: Arc<dyn ProviderEndpointValidator>,
    model_discoverer: &'a dyn ProviderModelDiscoverer,
}

fn respond_provider_probe_error(route: &LoginRoute, provider: &str, error: CatalogError) {
    let (failure, message, retryable) = match error {
        CatalogError::Transport { reason } => (ProviderProbeFailureWire::Unreachable, reason, true),
        CatalogError::Unauthorized => (
            ProviderProbeFailureWire::Unauthorized,
            "custom provider rejected the API key while listing models".to_owned(),
            false,
        ),
        CatalogError::InvalidBody { reason } => (
            ProviderProbeFailureWire::NonOpenAiCompatibleBody,
            reason,
            false,
        ),
        CatalogError::Empty => (
            ProviderProbeFailureWire::EmptyList,
            "custom provider returned an empty model list".to_owned(),
            false,
        ),
        CatalogError::Unavailable { reason } => {
            (ProviderProbeFailureWire::Unavailable, reason, false)
        }
        CatalogError::NotModified => (
            ProviderProbeFailureWire::Unavailable,
            "custom provider returned not-modified without a probe cache".to_owned(),
            true,
        ),
    };
    respond(
        route,
        ResponseBody::Error {
            code: ERROR_CODE_PROVIDER_ERROR.into(),
            message,
            retryable,
            data: Some(ErrorData::ProviderProbeFailed {
                provider: provider.to_owned(),
                failure,
            }),
        },
    );
}

async fn handle_provider_configure(
    context: ProviderConfigureContext<'_>,
    mut job: ProviderConfigureJob,
) {
    let ProviderConfigureContext {
        store,
        accounts,
        vault,
        management,
        providers,
        endpoint_validator,
        model_discoverer,
    } = context;
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
    let mut previous_auth_requirement = providers
        .get(&job.input.provider)
        .map(|profile| profile.auth_requirement);
    let preflight = store
        .management_receipt_preflight::<ProviderReceipt>(
            job.command_id.clone(),
            PROVIDER_CONFIGURE_METHOD.to_owned(),
            request_digest.clone(),
            request_json.clone(),
        )
        .await;
    let fresh_command = matches!(&preflight, Ok(None));
    if matches!(&preflight, Ok(None))
        && let Err(error) = check_expected_revision(store, job.expected_revision).await
    {
        respond_management_error(&job.route, &error);
        return;
    }
    let mut accepted_revision_unchanged = None;
    let mut recovered_catalog = None;
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
        Ok(Some(ManagementClaim::ResumePending {
            recovery_json: Some(recovery),
        })) => match serde_json::from_str::<ProviderConfigureRecovery>(&recovery) {
            Ok(recovery) => {
                job.input = recovery.input;
                previous_auth_requirement = recovery.previous_auth_requirement;
                if let Some(models) = recovery.discovered_models {
                    recovered_catalog = Some(DiscoveredCatalog {
                        models,
                        etag: recovery.discovered_etag,
                    });
                }
                accepted_revision_unchanged = Some(recovery.revision_unchanged);
            }
            Err(error) => {
                respond_management_error(
                    &job.route,
                    &HaiderError::new(
                        ErrorCode::StoreCorrupt,
                        format!("pending provider recovery coordinates are invalid: {error}"),
                        false,
                    ),
                );
                return;
            }
        },
        Ok(Some(ManagementClaim::Fresh | ManagementClaim::ResumePending { .. }) | None) => {}
        Err(error) => {
            respond_management_error(&job.route, &error);
            return;
        }
    }
    let (endpoint_to_validate, validate_repoint_claim) = match accepted_revision_unchanged {
        Some(_) => (None, false),
        None => match providers.get(&job.input.provider) {
            None => (job.input.origin.as_deref(), false),
            Some(profile) if matches!(profile.provenance, ProviderProvenance::Custom) => (
                job.input
                    .origin
                    .as_deref()
                    .filter(|origin| Some(*origin) != profile.base_url.as_deref()),
                true,
            ),
            Some(_) => (None, false),
        },
    };
    if let Some(origin) = endpoint_to_validate {
        if validate_repoint_claim
            && let Err(error) = providers.validate_repoint_origin_claim(&job.input.provider, origin)
        {
            respond_management_error(&job.route, &error);
            return;
        }
        let origin = origin.to_owned();
        let validation =
            tokio::spawn(async move { endpoint_validator.validate(&origin).await }).await;
        match validation {
            Ok(Ok(canonical_origin)) => job.input.origin = Some(canonical_origin),
            Ok(Err(error)) if error.code == ErrorCode::ProviderError => {
                respond_provider_probe_error(
                    &job.route,
                    &job.input.provider,
                    CatalogError::Transport {
                        reason: error.message,
                    },
                );
                return;
            }
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
    let mut discovered_slugs = recovered_catalog.as_ref().map(|catalog| {
        catalog
            .models
            .iter()
            .map(|model| model.slug.clone())
            .collect::<Vec<_>>()
    });
    let mut discovered_catalog = recovered_catalog;
    if accepted_revision_unchanged.is_none() {
        let existing = providers.get(&job.input.provider).cloned();
        let origin = job.input.origin.as_deref().or_else(|| {
            existing
                .as_ref()
                .and_then(|profile| profile.base_url.as_deref())
        });
        let should_discover = job.probe_secret.is_some() || job.input.models.is_empty();
        if should_discover {
            let Some(origin) = origin else {
                respond_error(
                    &job.route,
                    ERROR_CODE_INVALID_ARGUMENT,
                    "custom provider model discovery requires a base URL",
                    false,
                );
                return;
            };
            let family = job
                .input
                .api_family
                .or_else(|| existing.as_ref().map(|profile| profile.api_family));
            let source = match family {
                Some(ProviderApiFamilyWire::OpenAiChatCompletions) => {
                    CatalogSource::OpenAiCompatible {
                        origin: origin.to_owned(),
                    }
                }
                Some(ProviderApiFamilyWire::AnthropicMessages) => {
                    CatalogSource::AnthropicCompatible {
                        origin: origin.to_owned(),
                    }
                }
                _ => {
                    respond_error(
                        &job.route,
                        ERROR_CODE_INVALID_ARGUMENT,
                        "custom provider model discovery requires the openai or anthropic API family",
                        false,
                    );
                    return;
                }
            };
            let auth_requirement = job
                .input
                .auth_requirement
                .or_else(|| existing.as_ref().map(|profile| profile.auth_requirement));
            let discovery = match auth_requirement {
                Some(ProviderAuthRequirementWire::ApiKey) => {
                    if let Some(secret) = job.probe_secret.as_ref() {
                        match std::str::from_utf8(secret) {
                            Ok(secret) => {
                                model_discoverer.discover(source, Some(secret), None).await
                            }
                            Err(_) => {
                                respond_error(
                                    &job.route,
                                    ERROR_CODE_INVALID_ARGUMENT,
                                    "staged API key is not valid UTF-8",
                                    false,
                                );
                                return;
                            }
                        }
                    } else {
                        let Some(descriptor) = accounts.active_for_provider(&job.input.provider)
                        else {
                            respond_error(
                                &job.route,
                                ERROR_CODE_CREDENTIAL_MISSING,
                                "provider model discovery requires an API key",
                                false,
                            );
                            return;
                        };
                        let credential = match vault.resolve(&descriptor.alias) {
                            Ok(credential) => credential,
                            Err(error) => {
                                respond_error(
                                    &job.route,
                                    ERROR_CODE_CREDENTIAL_MISSING,
                                    &error.message,
                                    error.retryable,
                                );
                                return;
                            }
                        };
                        let secret = match std::str::from_utf8(credential.expose_secret()) {
                            Ok(secret) => secret,
                            Err(_) => {
                                respond_error(
                                    &job.route,
                                    ERROR_CODE_CREDENTIAL_MISSING,
                                    "stored API key is not valid UTF-8",
                                    false,
                                );
                                return;
                            }
                        };
                        model_discoverer.discover(source, Some(secret), None).await
                    }
                }
                Some(ProviderAuthRequirementWire::None) => {
                    if job.probe_secret.is_some() {
                        respond_error(
                            &job.route,
                            ERROR_CODE_INVALID_ARGUMENT,
                            "a no-auth provider must not carry a probe credential",
                            false,
                        );
                        return;
                    }
                    model_discoverer.discover(source, None, None).await
                }
                _ => {
                    respond_error(
                        &job.route,
                        ERROR_CODE_INVALID_ARGUMENT,
                        "custom provider model discovery requires api_key or no authentication",
                        false,
                    );
                    return;
                }
            };
            let catalog = match discovery {
                Ok(catalog) => catalog,
                Err(error) => {
                    respond_provider_probe_error(&job.route, &job.input.provider, error);
                    return;
                }
            };
            let slugs = catalog
                .models
                .iter()
                .map(|model| model.slug.clone())
                .collect::<Vec<_>>();
            let selected_default = job
                .input
                .default_model
                .as_ref()
                .filter(|model| slugs.contains(model))
                .cloned()
                .or_else(|| {
                    existing
                        .as_ref()
                        .and_then(|profile| profile.default_model.as_ref())
                        .filter(|model| slugs.contains(model))
                        .cloned()
                })
                .or_else(|| slugs.first().cloned());
            job.input.models = slugs.clone();
            job.input.default_model = selected_default;
            discovered_slugs = Some(slugs);
            discovered_catalog = Some(catalog);
        }
    }
    let custom_repoint = providers.get(&job.input.provider).is_some_and(|profile| {
        matches!(profile.provenance, ProviderProvenance::Custom) && job.input.origin.is_some()
    });
    let validate = match discovered_slugs.as_deref() {
        Some(inventory) => {
            providers.validate_configure_with_inventory(job.input.clone(), inventory)
        }
        None => providers.validate_configure(job.input.clone()),
    };
    let configuration_changed = match validate {
        Ok(changed) => changed,
        Err(error) => {
            respond_management_error(&job.route, &error);
            return;
        }
    };
    // A fresh configure that explicitly supplies the custom provider's
    // already-stored origin (including a spelling canonicalized by the
    // shared validator) is a semantic no-op. Its recovery coordinates embed
    // the public response so the store can atomically claim and commit the
    // receipt at the current revision; replay stays durable without opening
    // a pending-receipt window for an operation with no profile side effect.
    let mut revision_unchanged = accepted_revision_unchanged
        .unwrap_or(fresh_command && custom_repoint && !configuration_changed);
    let revision_unchanged_response = if revision_unchanged {
        match providers.summary(&job.input.provider, &provider_has_credential(accounts)) {
            Some(provider) => Some(ProviderReceipt {
                provider,
                revision_unchanged: true,
            }),
            None => {
                respond_management_error(
                    &job.route,
                    &HaiderError::new(
                        ErrorCode::StoreCorrupt,
                        "no-op provider configuration disappeared before receipt claim",
                        false,
                    ),
                );
                return;
            }
        }
    } else {
        None
    };
    let recovery_json = match serde_json::to_string(&ProviderConfigureRecovery {
        input: job.input.clone(),
        previous_auth_requirement,
        discovered_models: discovered_catalog
            .as_ref()
            .map(|catalog| catalog.models.clone()),
        discovered_etag: discovered_catalog
            .as_ref()
            .and_then(|catalog| catalog.etag.clone()),
        revision_unchanged,
        revision_unchanged_response,
    }) {
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
            if let Ok(recovery) = serde_json::from_str::<ProviderConfigureRecovery>(&recovery) {
                job.input = recovery.input;
                previous_auth_requirement = recovery.previous_auth_requirement;
                revision_unchanged = recovery.revision_unchanged;
            }
        }
        Ok(ManagementClaim::Fresh | ManagementClaim::ResumePending { .. }) => {}
        Err(error) => {
            respond_management_error(&job.route, &error);
            return;
        }
    }
    let auth_change = previous_auth_requirement
        .zip(job.input.auth_requirement)
        .filter(|(previous, current)| previous != current);
    let profile = if revision_unchanged {
        match providers.get(&job.input.provider).cloned() {
            Some(profile) => profile,
            None => {
                respond_management_error(
                    &job.route,
                    &HaiderError::new(
                        ErrorCode::StoreCorrupt,
                        "no-op provider configuration disappeared before finalization",
                        false,
                    ),
                );
                return;
            }
        }
    } else {
        let configured = match discovered_slugs.as_deref() {
            Some(inventory) => providers.configure_with_inventory(job.input, inventory),
            None => providers.configure(job.input),
        };
        match configured {
            Ok(profile) => profile,
            Err(error) => {
                respond_management_error(&job.route, &error);
                return;
            }
        }
    };
    let auth_event_revision = if let Some((previous, auth_requirement)) = auth_change {
        let Some(anticipated_revision) = job.expected_revision.checked_add(1) else {
            respond_management_error(
                &job.route,
                &HaiderError::new(
                    ErrorCode::StoreCorrupt,
                    "provider auth revision space is exhausted",
                    false,
                ),
            );
            return;
        };
        if let Err(error) = journal_provider_auth_changed(
            store,
            &job.command_id,
            &profile.provider_id,
            previous,
            auth_requirement,
            anticipated_revision,
        )
        .await
        {
            respond_management_error(&job.route, &error);
            return;
        }
        Some(anticipated_revision)
    } else {
        None
    };
    if let Some(catalog) = discovered_catalog {
        let models_json = match serde_json::to_string(&catalog.models) {
            Ok(models_json) => models_json,
            Err(error) => {
                respond_error(
                    &job.route,
                    ERROR_CODE_PROVIDER_ERROR,
                    &format!("could not encode provider model catalog: {error}"),
                    false,
                );
                return;
            }
        };
        let fetched_at_ms = unix_ms_after(Duration::ZERO);
        if let Err(error) = store
            .put_provider_models(
                profile.provider_id.clone(),
                models_json,
                catalog.etag,
                fetched_at_ms,
            )
            .await
        {
            respond_error(
                &job.route,
                ERROR_CODE_PROVIDER_ERROR,
                &error.message,
                error.retryable,
            );
            return;
        }
        providers.replace_models(
            profile.provider_id.clone(),
            catalog.models,
            Some(fetched_at_ms),
        );
    }
    let Some(provider) =
        providers.summary(&profile.provider_id, &provider_has_credential(accounts))
    else {
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
    let receipt = ProviderReceipt {
        provider,
        revision_unchanged,
    };
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
    if let Some(anticipated_revision) = auth_event_revision
        && revision != anticipated_revision
    {
        respond_management_error(
            &job.route,
            &HaiderError::new(
                ErrorCode::StoreCorrupt,
                format!(
                    "provider auth event revision {anticipated_revision} differs from committed revision {revision}"
                ),
                false,
            ),
        );
        return;
    }
    if !revision_unchanged && let Some(management) = management {
        management.publish(
            revision,
            accounts.list().to_vec(),
            providers.summaries(&provider_has_credential(accounts)),
        );
    }
    respond(
        &job.route,
        ResponseBody::ProviderConfigure {
            provider: receipt.provider,
            revision,
        },
    );
}

async fn handle_provider_remove(
    store: &SqliteStoreHandle,
    accounts: &AccountStore<Box<dyn StoreLike>>,
    management: Option<&ManagementSnapshot>,
    providers: &mut ProviderRegistry<Box<dyn ProviderRegistryStoreLike>>,
    refresh_in_progress: bool,
    job: ProviderRemoveJob,
) {
    let identity = ProviderRemoveIdentity {
        provider: job.provider.clone(),
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
        .management_receipt_preflight::<ProviderRemoveReceipt>(
            job.command_id.clone(),
            PROVIDER_REMOVE_METHOD.to_owned(),
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
    let resume_pending = match preflight {
        Ok(Some(ManagementClaim::Committed { response, revision })) => {
            respond(
                &job.route,
                ResponseBody::ProviderRemove {
                    provider: response.provider,
                    revision,
                },
            );
            return;
        }
        Ok(Some(ManagementClaim::Fresh)) => unreachable!("preflight never returns Fresh"),
        Ok(Some(ManagementClaim::ResumePending { .. })) => true,
        Ok(None) => {
            let Some(profile) = providers.get(&job.provider) else {
                respond_provider_remove_refused(
                    &job.route,
                    &job.provider,
                    ProviderRemoveRefusalReasonWire::NotFound,
                    Vec::new(),
                );
                return;
            };
            if !matches!(profile.provenance, ProviderProvenance::Custom) {
                respond_provider_remove_refused(
                    &job.route,
                    &job.provider,
                    ProviderRemoveRefusalReasonWire::ReleaseOwned,
                    Vec::new(),
                );
                return;
            }
            let blocking_aliases = provider_blocking_aliases(accounts, &job.provider);
            if !blocking_aliases.is_empty() {
                respond_provider_remove_refused(
                    &job.route,
                    &job.provider,
                    ProviderRemoveRefusalReasonWire::BlockingAccounts,
                    blocking_aliases,
                );
                return;
            }
            if refresh_in_progress {
                respond_management_error(
                    &job.route,
                    &HaiderError::new(
                        ErrorCode::Busy,
                        format!(
                            "provider `{}` has a model refresh in progress",
                            job.provider
                        ),
                        true,
                    ),
                );
                return;
            }
            false
        }
        Err(error) => {
            respond_management_error(&job.route, &error);
            return;
        }
    };
    match store
        .management_claim_receipt::<ProviderRemoveReceipt>(
            job.command_id.clone(),
            PROVIDER_REMOVE_METHOD.to_owned(),
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
                ResponseBody::ProviderRemove {
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
    let blocking_aliases = provider_blocking_aliases(accounts, &job.provider);
    if !blocking_aliases.is_empty() {
        respond_provider_remove_refused(
            &job.route,
            &job.provider,
            ProviderRemoveRefusalReasonWire::BlockingAccounts,
            blocking_aliases,
        );
        return;
    }
    let removed = if resume_pending {
        providers.reconcile_remove(&job.provider)
    } else {
        providers.remove_custom(&job.provider)
    };
    if let Err(error) = removed {
        respond_management_error(&job.route, &error);
        return;
    }
    let receipt = ProviderRemoveReceipt {
        provider: job.provider.clone(),
    };
    let revision = match store
        .finalize_provider_remove_receipt(job.command_id, job.provider.clone(), receipt.clone())
        .await
    {
        Ok(revision) => revision,
        Err(error) => {
            respond_management_error(&job.route, &error);
            return;
        }
    };
    if let Some(management) = management {
        management.publish(
            revision,
            accounts.list().to_vec(),
            providers.summaries(&provider_has_credential(accounts)),
        );
    }
    respond(
        &job.route,
        ResponseBody::ProviderRemove {
            provider: receipt.provider,
            revision,
        },
    );
}

async fn handle_provider_set_trust(
    store: &SqliteStoreHandle,
    accounts: &AccountStore<Box<dyn StoreLike>>,
    management: Option<&ManagementSnapshot>,
    providers: &mut ProviderRegistry<Box<dyn ProviderRegistryStoreLike>>,
    job: ProviderSetTrustJob,
) {
    let journal_command_id = job.command_id.clone();
    let identity = ProviderSetTrustIdentity {
        provider: job.provider.clone(),
        trust: job.trust,
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
            PROVIDER_SET_TRUST_METHOD.to_owned(),
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
                ResponseBody::ProviderSetTrust {
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
    if matches!(job.trust, ProviderTrustWire::Unknown) {
        respond_management_error(
            &job.route,
            &HaiderError::new(
                ErrorCode::InvalidArgument,
                "provider trust must be full or lockdown",
                false,
            ),
        );
        return;
    }
    let Some(previous_before_claim) = providers.get(&job.provider).map(|profile| profile.trust)
    else {
        respond_management_error(
            &job.route,
            &HaiderError::new(
                ErrorCode::InvalidArgument,
                format!("provider `{}` is not registered", job.provider),
                false,
            ),
        );
        return;
    };
    let recovery_json = match serde_json::to_string(&ProviderSetTrustRecovery {
        previous: previous_before_claim,
    }) {
        Ok(recovery_json) => recovery_json,
        Err(error) => {
            respond_management_error(
                &job.route,
                &HaiderError::new(
                    ErrorCode::Internal,
                    format!("cannot encode provider trust recovery: {error}"),
                    false,
                ),
            );
            return;
        }
    };
    let previous_trust = match store
        .management_claim_receipt::<ProviderReceipt>(
            job.command_id.clone(),
            PROVIDER_SET_TRUST_METHOD.to_owned(),
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
                ResponseBody::ProviderSetTrust {
                    provider: response.provider,
                    revision,
                },
            );
            return;
        }
        Ok(ManagementClaim::Fresh) => previous_before_claim,
        Ok(ManagementClaim::ResumePending { recovery_json }) => {
            match recovery_json.and_then(|json| serde_json::from_str(&json).ok()) {
                Some(ProviderSetTrustRecovery { previous }) => previous,
                None => {
                    respond_management_error(
                        &job.route,
                        &HaiderError::new(
                            ErrorCode::StoreCorrupt,
                            "pending provider-trust receipt has no recovery coordinates",
                            false,
                        ),
                    );
                    return;
                }
            }
        }
        Err(error) => {
            respond_management_error(&job.route, &error);
            return;
        }
    };
    let profile = match providers.set_trust(&job.provider, job.trust) {
        Ok(profile) => profile,
        Err(error) => {
            respond_management_error(&job.route, &error);
            return;
        }
    };
    let Some(provider) =
        providers.summary(&profile.provider_id, &provider_has_credential(accounts))
    else {
        respond_management_error(
            &job.route,
            &HaiderError::new(
                ErrorCode::StoreCorrupt,
                "trusted provider disappeared before receipt finalization",
                false,
            ),
        );
        return;
    };
    let receipt = ProviderReceipt {
        provider,
        revision_unchanged: false,
    };
    let Some(anticipated_revision) = job.expected_revision.checked_add(1) else {
        respond_management_error(
            &job.route,
            &HaiderError::new(
                ErrorCode::StoreCorrupt,
                "provider management revision space is exhausted",
                false,
            ),
        );
        return;
    };
    if let Err(error) = journal_provider_trust_changed(
        store,
        &journal_command_id,
        &job.provider,
        previous_trust,
        job.trust,
        anticipated_revision,
    )
    .await
    {
        respond_management_error(&job.route, &error);
        return;
    }
    let revision = match store
        .finalize_management_receipt(
            job.command_id,
            PROVIDER_SET_TRUST_METHOD.to_owned(),
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
    if revision != anticipated_revision {
        respond_management_error(
            &job.route,
            &HaiderError::new(
                ErrorCode::StoreCorrupt,
                format!(
                    "provider trust event revision {anticipated_revision} differs from committed revision {revision}"
                ),
                false,
            ),
        );
        return;
    }
    if let Some(management) = management {
        management.publish(
            revision,
            accounts.list().to_vec(),
            providers.summaries(&provider_has_credential(accounts)),
        );
    }
    respond(
        &job.route,
        ResponseBody::ProviderSetTrust {
            provider: receipt.provider,
            revision,
        },
    );
}

async fn journal_provider_trust_changed(
    store: &SqliteStoreHandle,
    command_id: &str,
    provider: &str,
    previous: ProviderTrustWire,
    trust: ProviderTrustWire,
    revision: u64,
) -> Result<(), HaiderError> {
    let payload = serde_json::to_value(EventPayload::ProviderTrustChanged(ProviderTrustChanged {
        provider: provider.to_owned(),
        previous: provider_trust_label(previous).to_owned(),
        trust: provider_trust_label(trust).to_owned(),
        revision,
    }))
    .map_err(|error| {
        HaiderError::new(
            ErrorCode::Internal,
            format!("cannot serialize provider trust event: {error}"),
            false,
        )
    })?;
    journal_provider_management_event(
        store,
        EventId::new(format!("provider-trust-{command_id}")),
        provider,
        payload,
    )
    .await
}

async fn journal_provider_auth_changed(
    store: &SqliteStoreHandle,
    command_id: &str,
    provider: &str,
    previous: ProviderAuthRequirementWire,
    auth_requirement: ProviderAuthRequirementWire,
    revision: u64,
) -> Result<(), HaiderError> {
    let payload = serde_json::to_value(EventPayload::ProviderAuthChanged(ProviderAuthChanged {
        provider: provider.to_owned(),
        previous: provider_auth_label(previous).to_owned(),
        auth_requirement: provider_auth_label(auth_requirement).to_owned(),
        revision,
    }))
    .map_err(|error| {
        HaiderError::new(
            ErrorCode::Internal,
            format!("cannot serialize provider auth event: {error}"),
            false,
        )
    })?;
    journal_provider_management_event(
        store,
        EventId::new(format!("provider-auth-{command_id}")),
        provider,
        payload,
    )
    .await
}

async fn journal_provider_management_event(
    store: &SqliteStoreHandle,
    event_id: EventId,
    provider: &str,
    payload: serde_json::Value,
) -> Result<(), HaiderError> {
    let profile_id = store.profile_installation_id().await?;
    for session_id in store.session_ids().await? {
        let Some(metadata) = store.session_metadata(&session_id).await? else {
            continue;
        };
        let bound_provider = match crate::lockdown::global() {
            Ok(manager) => manager
                .latest_session_provider(&profile_id, session_id.as_str())
                .map_err(|error| HaiderError::new(ErrorCode::Internal, error.to_string(), false))?,
            Err(_) => None,
        };
        if metadata.provider != provider && bound_provider.as_deref() != Some(provider) {
            continue;
        }
        let mut cursor = 0_u64;
        let mut already_journaled = false;
        loop {
            let page = StoreHandle::read(store, &session_id, cursor, 512).await?;
            if page.is_empty() {
                break;
            }
            already_journaled = page
                .iter()
                .any(|envelope| envelope.event_id == event_id && envelope.payload == payload);
            if already_journaled {
                break;
            }
            let Some(next) = page.last().map(|envelope| envelope.seq) else {
                break;
            };
            if next <= cursor {
                return Err(HaiderError::new(
                    ErrorCode::StoreCorrupt,
                    "provider management journal scan did not advance",
                    false,
                ));
            }
            cursor = next;
        }
        if already_journaled {
            continue;
        }
        let mut envelope = EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: event_id.clone(),
            seq: 0,
            session_id,
            branch_id: None,
            run_id: None,
            agent_id: None,
            device_id: DeviceId::new("provider-management"),
            authority_epoch: 0,
            worker_generation: store.worker_generation(),
            causation_id: None,
            correlation_id: None,
            committed_at_ms: 0,
            render: RenderTargets {
                ui: true,
                durable: true,
                prompt: PromptRender::Omit,
            },
            payload: payload.clone(),
        };
        StoreHandle::append(store, std::slice::from_mut(&mut envelope)).await?;
    }
    Ok(())
}

const fn provider_trust_label(trust: ProviderTrustWire) -> &'static str {
    match trust {
        ProviderTrustWire::Full => "full",
        ProviderTrustWire::Lockdown => "lockdown",
        ProviderTrustWire::Unknown => "unknown",
        _ => "unknown",
    }
}

const fn provider_auth_label(auth: ProviderAuthRequirementWire) -> &'static str {
    match auth {
        ProviderAuthRequirementWire::ApiKey => "api_key",
        ProviderAuthRequirementWire::None => "none",
        ProviderAuthRequirementWire::OAuth => "oauth",
        ProviderAuthRequirementWire::Unknown => "unknown",
        _ => "unknown",
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
    management: Option<&ManagementSnapshot>,
    providers: &ProviderRegistry<Box<dyn ProviderRegistryStoreLike>>,
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
        replace_existing,
        secret,
        route,
    } = job;
    // A custom profile validates against its OWN stored `/v1/models`
    // catalog; everything else keeps the fixed validator set.
    let custom_target = custom_login_target(management, &provider);
    // G4b: the enterprise builtins validate at their PROFILE endpoint with
    // the profile's declared default model spelling.
    let enterprise_target = enterprise_login_target(management, &provider);
    if !validator.supports(&provider) && custom_target.is_none() {
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
        // A custom profile's DECLARED default outranks the global one —
        // validating a local server against the global (vendor) model id
        // would 404 on every honest server.
        resolved_model: validation_model.unwrap_or_else(|| {
            custom_target
                .as_ref()
                .and_then(|(_, default, _)| default.clone())
                .or_else(|| {
                    enterprise_target
                        .as_ref()
                        .and_then(|(_, default)| default.clone())
                })
                .unwrap_or_else(|| default_model.to_owned())
        }),
        display_alias: Some(selected_alias.clone()),
        physical_alias: selected_alias,
        replace_existing,
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
    // Prefer a still-live command-owned secret over crash reconciliation.
    // This matters for an in-place re-key: the old descriptor legitimately
    // exists while a retryable validation is pending, so treating descriptor
    // presence as proof of commit would finalize the old key instead of
    // retrying the replacement.
    let secret = secret.or_else(|| {
        pending
            .remove(&command_id)
            .filter(|entry| entry.claimed_at.elapsed() < SECRET_TTL)
            .map(|entry| entry.secret)
    });
    if resume {
        // Crash-boundary reconciliation at command time (R10 step 10):
        // descriptor present -> finalize; vault-only -> resume descriptor
        // commit; neither -> continue with a fresh stage below.
        if accounts.get(&alias).is_some() && secret.is_none() && !replace_existing {
            drop(secret);
            pending.remove(&command_id);
            finalize_and_respond(
                store,
                accounts,
                providers,
                snapshot,
                management,
                &command_id,
                &alias,
                &route,
            )
            .await;
            return;
        }
        if !replace_existing && accounts.get(&alias).is_none() && vault.resolve(&alias).is_ok() {
            drop(secret);
            pending.remove(&command_id);
            let descriptor = descriptor_for(&identity, &alias, None, None);
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
                providers,
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
        None => {
            respond_error(
                &route,
                ERROR_CODE_RESTAGE_REQUIRED,
                "staged secret is no longer available; stage the key again and retry",
                true,
            );
            return;
        }
    };

    let validation = match &custom_target {
        Some((origin, _, api_family)) => {
            validate_custom_provider_key(origin, &provider, *api_family, &secret).await
        }
        None => {
            validator
                .validate(
                    &provider,
                    &identity.resolved_model,
                    &secret,
                    enterprise_target
                        .as_ref()
                        .map(|(origin, _)| origin.as_str()),
                )
                .await
        }
    };
    match validation {
        Ok(validated) => {
            let replacing = replace_existing || accounts.get(&alias).is_some();
            let prior_secret = if replacing {
                vault.resolve(&alias).ok()
            } else {
                None
            };
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
            let account_identity = api_key_identity(&provider, &secret);
            drop(secret);
            pending.remove(&command_id);
            let descriptor = descriptor_for(
                &identity,
                &alias,
                Some(validated.identity),
                Some(account_identity),
            );
            let descriptor_result = if replacing {
                accounts.replace(descriptor)
            } else {
                accounts.add(descriptor)
            };
            if let Err(error) = descriptor_result {
                // Synchronous descriptor-save failure deletes the
                // just-written vault alias on add, or restores the prior
                // bytes on an in-place re-key. The receipt stays pending and
                // a fresh stage retries.
                let rollback = match prior_secret {
                    Some(previous) => vault.put(&alias, previous.expose_secret()),
                    None => vault.delete(&alias),
                };
                if rollback.is_err() {
                    respond_error(
                        &route,
                        ERROR_CODE_PROVIDER_ERROR,
                        "descriptor save and vault rollback failed",
                        true,
                    );
                    return;
                }
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
                providers,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    candidate: Option<String>,
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

#[derive(Default)]
struct OAuthImportHealSink {
    result: StdMutex<Option<Result<CredentialDescriptor, HaiderError>>>,
}

impl OAuthImportHealSink {
    fn take(&self) -> Option<Result<CredentialDescriptor, HaiderError>> {
        self.result.lock().ok()?.take()
    }
}

impl FrameSink for OAuthImportHealSink {
    fn try_send(&self, frame: WireFrame) -> Result<(), crate::session_hub::FrameSendError> {
        let result = match frame {
            WireFrame::Response {
                body: ResponseBody::AccountOAuthImport { descriptor, .. },
                ..
            } => Ok(descriptor),
            WireFrame::Response {
                body:
                    ResponseBody::Error {
                        message, retryable, ..
                    },
                ..
            } => Err(HaiderError::new(
                ErrorCode::ProviderError,
                message,
                retryable,
            )),
            _ => Err(HaiderError::new(
                ErrorCode::Internal,
                "OAuth self-heal import returned an unexpected response",
                false,
            )),
        };
        let mut slot = self
            .result
            .lock()
            .map_err(|_| crate::session_hub::FrameSendError)?;
        *slot = Some(result);
        Ok(())
    }
}

fn fresh_oauth_heal_command_id() -> Result<String, HaiderError> {
    let mut random = [0_u8; 32];
    getrandom::fill(&mut random).map_err(|_| {
        HaiderError::new(
            ErrorCode::Internal,
            "cannot mint an OAuth self-heal command id",
            true,
        )
    })?;
    Ok(format!("oauth-heal-{}", blake3::hash(&random).to_hex()))
}

#[allow(clippy::too_many_arguments)]
async fn handle_device_import(
    store: &SqliteStoreHandle,
    accounts: &mut AccountStore<Box<dyn StoreLike>>,
    vault: Arc<dyn Vault>,
    snapshot: &AccountsSnapshot,
    management: Option<&ManagementSnapshot>,
    providers: &ProviderRegistry<Box<dyn ProviderRegistryStoreLike>>,
    reserved_aliases: &HashSet<String>,
    refresh_fences: &RefreshFenceRegistry,
    gcloud: Arc<dyn crate::gcloud::GcloudAccessTokenSource>,
    claude_native: Arc<dyn ClaudeNativeCredentialStore>,
    job: DeviceImportJob,
) {
    let candidate_id = job.candidate.clone();
    let disabled = job.discovery_disabled;
    let candidate =
        match find_device_candidate(Arc::clone(&claude_native), disabled, candidate_id).await {
            Ok(candidate) => candidate,
            Err(error) => {
                respond_management_error(&job.route, &error);
                return;
            }
        };
    let Some(candidate) = candidate else {
        respond_management_error(
            &job.route,
            &HaiderError::new(
                ErrorCode::InvalidArgument,
                if crate::device_discovery::discovery_is_disabled(disabled) {
                    "device credential discovery is disabled"
                } else {
                    "device credential candidate is unavailable"
                },
                false,
            ),
        );
        return;
    };
    let Some(source) = candidate.import_source else {
        respond_management_error(
            &job.route,
            &HaiderError::new(
                ErrorCode::InvalidArgument,
                candidate
                    .wire
                    .unsupported_reason
                    .unwrap_or_else(|| "device credential cannot be imported".to_owned()),
                false,
            ),
        );
        return;
    };
    let expected_source_fingerprint = candidate.content_fingerprint;
    // G4b (LV2): the gcloud candidate imports through the SHELL-OUT source,
    // not an OAuth bundle file — its own arm, before the bundle machinery.
    if source == crate::device_discovery::GCLOUD_IMPORT_SOURCE {
        handle_gcloud_import(
            store, accounts, vault, snapshot, management, providers, gcloud, job,
        )
        .await;
        return;
    }
    let receipt_candidate = Some(job.candidate.clone());
    handle_oauth_import(
        store,
        accounts,
        vault,
        snapshot,
        management,
        reserved_aliases,
        refresh_fences,
        OAuthImportJob {
            command_id: job.command_id,
            source: source.to_owned(),
            route: job.route,
        },
        None,
        OAuthCommitResponse::ImportDevice,
        receipt_candidate,
        expected_source_fingerprint,
        ClaudeNativeReadEvent::Significant,
        claude_native,
    )
    .await;
}

fn is_probe_account_alias(alias: &str) -> bool {
    if matches!(alias, "probefix" | "probefix-api") {
        return true;
    }
    if let Some(index) = alias.strip_prefix("probefix-api-") {
        return canonical_positive_decimal(index) && index != "1";
    }
    alias
        .strip_prefix("probe")
        .and_then(|suffix| suffix.strip_suffix("-api"))
        .is_some_and(canonical_positive_decimal)
}

fn canonical_positive_decimal(value: &str) -> bool {
    value
        .as_bytes()
        .first()
        .is_some_and(|first| matches!(first, b'1'..=b'9'))
        && value.as_bytes()[1..].iter().all(u8::is_ascii_digit)
}

async fn discover_candidates(
    claude_native: Arc<dyn ClaudeNativeCredentialStore>,
    discovery_disabled: bool,
    event: ClaudeNativeReadEvent,
) -> Result<Vec<crate::device_discovery::DeviceCandidate>, HaiderError> {
    discover_candidates_with_deadline(
        claude_native,
        discovery_disabled,
        event,
        DEVICE_DISCOVERY_WORKER_TIMEOUT,
    )
    .await
}

async fn discover_candidates_with_deadline(
    claude_native: Arc<dyn ClaudeNativeCredentialStore>,
    discovery_disabled: bool,
    event: ClaudeNativeReadEvent,
    deadline: Duration,
) -> Result<Vec<crate::device_discovery::DeviceCandidate>, HaiderError> {
    let native_for_discovery = Arc::clone(&claude_native);
    let worker = tokio::task::spawn_blocking(move || {
        crate::device_discovery::discover_device_candidates_with_native_event(
            discovery_disabled,
            native_for_discovery.as_ref(),
            event,
        )
    });
    // Discovery is metadata-only. Import is exclusively the explicit,
    // Control-authorized account.import_device/account.oauth_import door.
    await_device_discovery_worker(worker, deadline).await
}

async fn find_device_candidate(
    claude_native: Arc<dyn ClaudeNativeCredentialStore>,
    discovery_disabled: bool,
    candidate_id: String,
) -> Result<Option<crate::device_discovery::DeviceCandidate>, HaiderError> {
    let worker = tokio::task::spawn_blocking(move || {
        crate::device_discovery::candidate_by_id_with_native(
            discovery_disabled,
            &candidate_id,
            claude_native.as_ref(),
        )
    });
    await_device_discovery_worker(worker, DEVICE_DISCOVERY_WORKER_TIMEOUT).await
}

async fn await_device_discovery_worker<T: Send + 'static>(
    worker: tokio::task::JoinHandle<T>,
    deadline: Duration,
) -> Result<T, HaiderError> {
    match tokio::time::timeout(deadline, worker).await {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(_)) => Err(device_discovery_worker_error("worker was lost")),
        Err(_) => Err(device_discovery_worker_error("deadline elapsed")),
    }
}

fn device_discovery_worker_error(reason: &str) -> HaiderError {
    HaiderError::new(
        ErrorCode::ProviderError,
        format!("device credential discovery unavailable: {reason}"),
        true,
    )
}

/// Imports (or refreshes) the vertex gcloud credential (G4b, LV2): run the
/// mockable shell-out, vault the token under the fixed `vertex-gcloud`
/// alias, and upsert the descriptor. DELIBERATELY receipt-free: re-import
/// IS refresh (each run mints a fresh short-lived token), so replaying the
/// command after a crash or retry is idempotent by construction, and no
/// receipt may carry a secret anyway — the vault file is the durable truth
/// (the transcription-secret precedent).
#[allow(clippy::too_many_arguments)]
async fn handle_gcloud_import(
    store: &SqliteStoreHandle,
    accounts: &mut AccountStore<Box<dyn StoreLike>>,
    vault: Arc<dyn Vault>,
    snapshot: &AccountsSnapshot,
    management: Option<&ManagementSnapshot>,
    providers: &ProviderRegistry<Box<dyn ProviderRegistryStoreLike>>,
    gcloud: Arc<dyn crate::gcloud::GcloudAccessTokenSource>,
    job: DeviceImportJob,
) {
    let token = match tokio::task::spawn_blocking(move || gcloud.print_access_token()).await {
        Ok(Ok(token)) => token,
        Ok(Err(error)) => {
            respond_management_error(&job.route, &error);
            return;
        }
        Err(_) => {
            respond_management_error(
                &job.route,
                &crate::gcloud::gcloud_error("import worker was lost"),
            );
            return;
        }
    };
    let account_identity = api_key_identity(haider_provider::VERTEX_PROVIDER_NAME, &token);
    let alias = CredentialAlias::new(crate::gcloud::VERTEX_GCLOUD_ALIAS);
    let vault_for_write = Arc::clone(&vault);
    let alias_for_write = alias.clone();
    let written =
        tokio::task::spawn_blocking(move || vault_for_write.put(&alias_for_write, &token)).await;
    match written {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            respond_management_error(&job.route, &error);
            return;
        }
        Err(_) => {
            respond_management_error(
                &job.route,
                &crate::gcloud::gcloud_error("vault worker was lost"),
            );
            return;
        }
    }
    let refreshed = accounts.get(&alias).is_some();
    let result = if refreshed {
        // Re-import refreshed the vault token; the descriptor only needs
        // its status healed.
        accounts
            .set_status(&alias, CredentialStatus::Ok)
            .and_then(|()| {
                accounts
                    .set_identity(&alias, Some(account_identity))
                    .map(|_| ())
            })
    } else {
        accounts.add(CredentialDescriptor {
            alias: alias.clone(),
            provider: haider_provider::VERTEX_PROVIDER_NAME.to_owned(),
            base_url: None,
            auth_method: AuthMethod::ApiKey,
            identity: "gcloud access token (auto-refresh)".to_owned(),
            status: CredentialStatus::Ok,
            active: true,
            label: None,
            account_identity: Some(account_identity),
            created_at_ms: None,
        })
    };
    if let Err(error) = result {
        respond_management_error(&job.route, &error);
        return;
    }
    let revision = match store.advance_management_revision().await {
        Ok(revision) => revision,
        Err(error) => {
            respond_management_error(&job.route, &error);
            return;
        }
    };
    // The fresh credential flips the vertex seeded-inventory availability
    // (decision 6) — publish the FULL provider view, not just accounts.
    refresh_resolver_snapshot(snapshot, accounts);
    if let Some(management) = management {
        management.publish(
            revision,
            accounts.list().to_vec(),
            providers.summaries(&provider_has_credential(accounts)),
        );
    }
    let Some(descriptor) = accounts.get(&alias).cloned() else {
        respond_management_error(
            &job.route,
            &HaiderError::new(
                ErrorCode::Internal,
                "gcloud import descriptor disappeared before the response",
                false,
            ),
        );
        return;
    };
    respond(
        &job.route,
        ResponseBody::AccountImportDevice {
            descriptor,
            revision,
        },
    );
}

#[allow(clippy::too_many_arguments)]
async fn handle_oauth_import_heal(
    store: &SqliteStoreHandle,
    accounts: &mut AccountStore<Box<dyn StoreLike>>,
    vault: Arc<dyn Vault>,
    snapshot: &AccountsSnapshot,
    management: Option<&ManagementSnapshot>,
    reserved_aliases: &HashSet<String>,
    refresh_fences: &RefreshFenceRegistry,
    descriptor: &CredentialDescriptor,
    expected: &OAuthRefreshFence,
    claude_native: Arc<dyn ClaudeNativeCredentialStore>,
) -> Result<OAuthImportHealResult, HaiderError> {
    if refresh_fences.current(&descriptor.alias) != expected.fence_epoch
        || !accounts
            .get(&descriptor.alias)
            .is_some_and(|current| same_credential_identity(current, descriptor))
    {
        return Ok(OAuthImportHealResult::NotImported);
    }
    let receipts = store.account_add_receipts().await?;
    let latest = latest_oauth_receipt_identities(&receipts);
    let Some((_, OAuthReceiptIdentity::Import(identity))) = latest.get(descriptor.alias.as_str())
    else {
        return Ok(OAuthImportHealResult::NotImported);
    };
    if identity.alias != descriptor.alias.as_str() || identity.provider != descriptor.provider {
        return Ok(OAuthImportHealResult::NotImported);
    }
    let spec = oauth_import_source_spec(&identity.source)?;
    if spec.provider != descriptor.provider {
        return Ok(OAuthImportHealResult::NotImported);
    }
    let source = identity.source.clone();
    let vault_for_read = Arc::clone(&vault);
    let alias_for_read = descriptor.alias.clone();
    let current = tokio::task::spawn_blocking(move || vault_for_read.resolve(&alias_for_read))
        .await
        .map_err(|_| {
            HaiderError::new(ErrorCode::ProviderError, "OAuth vault worker failed", true)
        })??;
    let current = haider_accounts::OAuthTokenBundleV1::decode(current.expose_secret())?;
    if current.provider_id != descriptor.provider
        || current.issuer != expected.issuer
        || current.audience != expected.audience
        || current.resource != expected.resource
        || current.identity.subject_hash != expected.subject_hash
        || current.identity.display_identity != descriptor.identity
    {
        return Ok(OAuthImportHealResult::NotImported);
    }
    let linked_native_owner = is_claude_native_owner_identity(&current.identity.display_identity);
    if current.generation != expected.generation {
        // The receipt has already proved this physical alias is a Codex
        // import. A contender may have read generation N before another
        // daemon durably committed N+1, then reached this actor only after
        // that commit. Preserve import provenance so the contender enters
        // the serialized refresh path: its under-lease re-read adopts N+1
        // instead of attempting a conservative refresh with stale N.
        if source == "claude-code"
            && descriptor.provider == ANTHROPIC_OAUTH_PROVIDER_NAME
            && linked_native_owner
        {
            let native_for_read = Arc::clone(&claude_native);
            let read = tokio::task::spawn_blocking(move || {
                native_for_read.read(ClaudeNativeReadEvent::Ordinary)
            })
            .await
            .map_err(|_| {
                HaiderError::new(
                    ErrorCode::ProviderError,
                    "Claude Code credential-store worker failed",
                    true,
                )
            })?;
            return match read {
                Ok(_) => Ok(OAuthImportHealResult::LiveOwnerStore { source }),
                Err(failure) => {
                    if unix_ms_after(Duration::ZERO) >= current.expires_at_unix_ms {
                        persist_native_store_attention(
                            store,
                            accounts,
                            snapshot,
                            management,
                            &descriptor.alias,
                            failure,
                        )
                        .await?;
                    }
                    Ok(OAuthImportHealResult::LiveOwnerUnavailable { failure })
                }
            };
        }
        return Ok(OAuthImportHealResult::RefreshFallback { source });
    }
    let Some(generation) = current.generation.checked_add(1) else {
        return Err(HaiderError::new(
            ErrorCode::ProviderError,
            "OAuth token generation is exhausted",
            false,
        ));
    };
    let source_for_read = source.clone();
    let native_for_read = Arc::clone(&claude_native);
    let imported =
        if source == "claude-code" && descriptor.provider == ANTHROPIC_OAUTH_PROVIDER_NAME {
            match tokio::task::spawn_blocking(move || {
                load_claude_native_import_material(
                    generation,
                    native_for_read.as_ref(),
                    ClaudeNativeReadEvent::Ordinary,
                )
            })
            .await
            {
                Ok(Ok(imported)) => imported,
                // A present but malformed/invalid owner store is still owner
                // controlled. Returning the parse error is fail-closed: the
                // snapshotted rotating refresh token remains untouched.
                Ok(Err(ClaudeNativeImportError::Invalid(error))) => return Err(error),
                Err(_) => {
                    return Err(HaiderError::new(
                        ErrorCode::ProviderError,
                        "Claude Code credential-store worker failed",
                        true,
                    ));
                }
                Ok(Err(ClaudeNativeImportError::Access(failure))) if linked_native_owner => {
                    if unix_ms_after(Duration::ZERO) >= current.expires_at_unix_ms {
                        persist_native_store_attention(
                            store,
                            accounts,
                            snapshot,
                            management,
                            &descriptor.alias,
                            failure,
                        )
                        .await?;
                    }
                    return Ok(OAuthImportHealResult::LiveOwnerUnavailable { failure });
                }
                Ok(Err(ClaudeNativeImportError::Access(_))) => {
                    let native_for_fallback = Arc::clone(&claude_native);
                    match tokio::task::spawn_blocking(move || {
                        load_oauth_import_material_with_native(
                            &source_for_read,
                            generation,
                            native_for_fallback.as_ref(),
                            ClaudeNativeReadEvent::Ordinary,
                        )
                    })
                    .await
                    {
                        Ok(Ok(imported)) => imported,
                        Ok(Err(_)) | Err(_) => {
                            return Ok(OAuthImportHealResult::RefreshFallback { source });
                        }
                    }
                }
            }
        } else {
            match tokio::task::spawn_blocking(move || {
                load_oauth_import_material_with_native(
                    &source_for_read,
                    generation,
                    native_for_read.as_ref(),
                    ClaudeNativeReadEvent::Ordinary,
                )
            })
            .await
            {
                Ok(Ok(imported)) => imported,
                Ok(Err(_)) | Err(_) => {
                    return Ok(OAuthImportHealResult::RefreshFallback { source });
                }
            }
        };
    let live_owner = imported.claude_native_owner;
    if same_oauth_import(&current, &imported.bundle)
        && !matches!(descriptor.status, CredentialStatus::NeedsAttention { .. })
    {
        if live_owner {
            // A successful owner read always refreshes Haider's degraded-path
            // snapshot, even when token bytes are unchanged. Keep the current
            // generation because no descriptor/revision mutation occurred.
            let mut bundle = imported.bundle;
            bundle.generation = current.generation;
            let encoded = bundle.encode()?;
            let vault_for_put = Arc::clone(&vault);
            let alias_for_put = descriptor.alias.clone();
            tokio::task::spawn_blocking(move || vault_for_put.put(&alias_for_put, &encoded))
                .await
                .map_err(|_| {
                    HaiderError::new(
                        ErrorCode::ProviderError,
                        "Claude snapshot persistence worker failed",
                        true,
                    )
                })??;
        }
        return Ok(if live_owner {
            OAuthImportHealResult::LiveOwnerStore { source }
        } else {
            OAuthImportHealResult::RefreshFallback { source }
        });
    }
    let source_access_fingerprint = *blake3::hash(imported.bundle.access_token()).as_bytes();
    if !live_owner
        && (current.import_source_access_fingerprint() == Some(source_access_fingerprint)
            || (current.import_source_access_fingerprint().is_none() && current.generation > 1))
    {
        // The external CLI may still hold the exact source generation Haider
        // imported before its own broker refresh rotated the shared token.
        // Never "heal" by writing that spent predecessor back over the
        // durable successor. Historical generation>1 bundles without the C2
        // fingerprint are directionally ambiguous, so they also fail safe to
        // the serialized refresh path instead of risking rollback.
        return Ok(OAuthImportHealResult::RefreshFallback { source });
    }
    let mut committed = OAuthRefreshFence {
        fence_epoch: expected.fence_epoch,
        generation: imported.bundle.generation,
        issuer: imported.bundle.issuer.clone(),
        audience: imported.bundle.audience.clone(),
        resource: imported.bundle.resource.clone(),
        subject_hash: imported.bundle.identity.subject_hash.clone(),
    };
    let command_id = fresh_oauth_heal_command_id()?;
    let sink = Arc::new(OAuthImportHealSink::default());
    let route_sink: Arc<dyn FrameSink> = sink.clone();
    handle_oauth_import(
        store,
        accounts,
        vault,
        snapshot,
        management,
        reserved_aliases,
        refresh_fences,
        OAuthImportJob {
            command_id: command_id.clone(),
            source: source.clone(),
            route: LoginRoute {
                request_id: RequestId::new(command_id),
                sink: route_sink,
            },
        },
        Some(imported),
        OAuthCommitResponse::ImportLegacy,
        None,
        None,
        ClaudeNativeReadEvent::Ordinary,
        claude_native,
    )
    .await;
    let committed_descriptor = sink.take().ok_or_else(|| {
        HaiderError::new(
            ErrorCode::Internal,
            "OAuth self-heal import lost its completion",
            false,
        )
    })??;
    if committed_descriptor.alias != descriptor.alias
        || committed_descriptor.provider != spec.provider
    {
        return Err(HaiderError::new(
            ErrorCode::Internal,
            "OAuth self-heal import committed an unexpected account",
            false,
        ));
    }
    committed.fence_epoch = refresh_fences.current(&committed_descriptor.alias);
    Ok(OAuthImportHealResult::Committed {
        expected: committed,
    })
}

fn native_attention_reason(failure: ClaudeNativeCredentialFailure) -> CredentialAttentionReason {
    match failure {
        ClaudeNativeCredentialFailure::Denied => CredentialAttentionReason::KeychainDenied,
        ClaudeNativeCredentialFailure::Locked => CredentialAttentionReason::KeychainLocked,
        ClaudeNativeCredentialFailure::Missing => CredentialAttentionReason::KeychainMissing,
        ClaudeNativeCredentialFailure::Unavailable => {
            CredentialAttentionReason::KeychainUnavailable
        }
        ClaudeNativeCredentialFailure::TimedOut => CredentialAttentionReason::KeychainUnavailable,
    }
}

async fn persist_native_store_attention(
    store: &SqliteStoreHandle,
    accounts: &mut AccountStore<Box<dyn StoreLike>>,
    snapshot: &AccountsSnapshot,
    management: Option<&ManagementSnapshot>,
    alias: &CredentialAlias,
    failure: ClaudeNativeCredentialFailure,
) -> Result<(), HaiderError> {
    let status = CredentialStatus::NeedsAttention {
        reason: native_attention_reason(failure),
    };
    if accounts
        .get(alias)
        .is_some_and(|descriptor| descriptor.status == status)
    {
        return Ok(());
    }
    accounts.set_status(alias, status)?;
    refresh_resolver_snapshot(snapshot, accounts);
    publish_next_management_revision(store, snapshot, management, accounts)
        .await
        .map(|_| ())
}

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
    preloaded_material: Option<OAuthImportMaterial>,
    response_kind: OAuthCommitResponse,
    receipt_candidate: Option<String>,
    expected_source_fingerprint: Option<[u8; 32]>,
    native_read_event: ClaudeNativeReadEvent,
    claude_native: Arc<dyn ClaudeNativeCredentialStore>,
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
        candidate: receipt_candidate,
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
                oauth_import_response(response_kind, response.descriptor, revision),
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
                oauth_import_response(response_kind, response.descriptor, revision),
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
            // The descriptor row exists but its secret is GONE (a
            // Keychain→file-vault upgrade, W5f-4; or a torn write). Re-import
            // is exactly the recovery: proceed as if fresh — no prior bundle
            // to increment a generation from, and the new secret restores the
            // account. Any OTHER vault fault is real and still fails.
            Ok(Err(error)) if error.code == ErrorCode::CredentialMissing => None,
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
    let imported = match preloaded_material {
        Some(imported) if imported.bundle.generation == generation => imported,
        Some(_) => {
            respond_management_error(
                &job.route,
                &HaiderError::new(
                    ErrorCode::ProviderError,
                    "OAuth import generation changed before commit",
                    true,
                ),
            );
            return;
        }
        None => {
            let claude_native = Arc::clone(&claude_native);
            match tokio::task::spawn_blocking(move || {
                load_oauth_import_material_with_native(
                    &source,
                    generation,
                    claude_native.as_ref(),
                    native_read_event,
                )
            })
            .await
            {
                Ok(Ok(material)) => material,
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
            }
        }
    };
    if imported.bundle.provider_id != spec.provider {
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
    if !confirmed_source_matches(expected_source_fingerprint, imported.source_fingerprint) {
        respond_management_error(
            &job.route,
            &HaiderError::new(
                ErrorCode::InvalidArgument,
                "device credential changed after confirmation; review it and confirm import again",
                false,
            ),
        );
        return;
    }
    if resume
        && let Some(prior) = prior_bundle.as_ref()
        && same_oauth_import(prior, &imported.bundle)
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
            response_kind,
        )
        .await;
        return;
    }
    if replacing.is_some() {
        refresh_fences.invalidate(&alias);
    }
    if let Err(error) = persist_oauth_import_material(
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
        response_kind,
    )
    .await;
}

fn confirmed_source_matches(expected: Option<[u8; 32]>, actual: [u8; 32]) -> bool {
    expected.is_none_or(|expected| expected == actual)
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
                    && !is_probe_account_alias(&identity.alias)
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
    let default = CredentialAlias::new(default_alias);
    if accounts.get(&default).is_some_and(|descriptor| {
        descriptor.provider == provider
            && descriptor.auth_method == AuthMethod::OAuth
            && matches!(
                descriptor.status,
                CredentialStatus::Expired | CredentialStatus::NeedsAttention { .. }
            )
    }) {
        // A source-less, expired default OAuth slot is safe to repair in
        // place. Healthy/manual incarnations retain the ownership protection
        // below and force a suffixed import instead.
        return Ok(default);
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
    // The STORE is the authority on whether the descriptor row exists —
    // not `prior_secret`. A vanished prior secret (a Keychain→file-vault
    // upgrade, W5f-4; or a torn write) still has a live descriptor row, and
    // that row must be REPLACED, not add-rejected as a duplicate. Rollback
    // below still keys off `prior_secret`: with none, a failed save deletes
    // what we just wrote rather than restoring bytes that were already gone.
    let replacing = accounts.get(alias).is_some();
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

async fn persist_oauth_import_material(
    accounts: &mut AccountStore<Box<dyn StoreLike>>,
    vault: Arc<dyn Vault>,
    provider: &str,
    alias: &CredentialAlias,
    material: OAuthImportMaterial,
    prior_secret: Option<SecretHandle>,
) -> Result<(), HaiderError> {
    let Some(device_id) = material.kimi_device_id else {
        return persist_oauth_bundle(
            accounts,
            vault,
            provider,
            alias,
            material.bundle,
            prior_secret,
        )
        .await;
    };
    let device_alias = CredentialAlias::new(KIMI_DEVICE_ALIAS);
    let vault_for_read = Arc::clone(&vault);
    let alias_for_read = device_alias.clone();
    let prior_device =
        match tokio::task::spawn_blocking(move || vault_for_read.resolve(&alias_for_read))
            .await
            .map_err(|_| {
                HaiderError::new(ErrorCode::ProviderError, "OAuth vault worker failed", true)
            })? {
            Ok(secret) => Some(secret),
            Err(error) if error.code == ErrorCode::CredentialMissing => None,
            Err(error) => return Err(error),
        };
    let vault_for_put = Arc::clone(&vault);
    let alias_for_put = device_alias.clone();
    tokio::task::spawn_blocking(move || vault_for_put.put(&alias_for_put, &device_id))
        .await
        .map_err(|_| {
            HaiderError::new(ErrorCode::ProviderError, "OAuth vault worker failed", true)
        })??;
    if let Err(error) = persist_oauth_bundle(
        accounts,
        Arc::clone(&vault),
        provider,
        alias,
        material.bundle,
        prior_secret,
    )
    .await
    {
        let rollback_vault = Arc::clone(&vault);
        let rollback_alias = device_alias;
        let rollback = tokio::task::spawn_blocking(move || match prior_device {
            Some(previous) => rollback_vault.put(&rollback_alias, previous.expose_secret()),
            None => rollback_vault.delete(&rollback_alias),
        })
        .await;
        if !matches!(rollback, Ok(Ok(()))) {
            return Err(HaiderError::new(
                ErrorCode::ProviderError,
                "OAuth import and device-identity rollback failed",
                true,
            ));
        }
        return Err(error);
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
        label: None,
        account_identity: bundle.account_identity.clone(),
        // AccountStore::add owns the record-creation timestamp. Replacement
        // preserves the prior value, including None for pre-964 rows.
        created_at_ms: None,
    }
}

#[derive(Clone, Copy)]
enum OAuthCommitResponse {
    Add,
    ImportLegacy,
    ImportDevice,
}

fn oauth_import_response(
    response: OAuthCommitResponse,
    descriptor: CredentialDescriptor,
    revision: u64,
) -> ResponseBody {
    match response {
        OAuthCommitResponse::ImportLegacy => ResponseBody::AccountOAuthImport {
            descriptor,
            revision,
        },
        OAuthCommitResponse::ImportDevice => ResponseBody::AccountImportDevice {
            descriptor,
            revision,
        },
        OAuthCommitResponse::Add => unreachable!("OAuth add is not an import response"),
    }
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
            OAuthCommitResponse::ImportLegacy | OAuthCommitResponse::ImportDevice => {
                respond_management_error(route, &error);
            }
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
                OAuthCommitResponse::ImportLegacy | OAuthCommitResponse::ImportDevice => {
                    respond_management_error(route, &error);
                }
            }
            return;
        }
    };
    publish_management_snapshot(snapshot, management, accounts, revision);
    match response {
        OAuthCommitResponse::Add => respond(route, ResponseBody::AccountAdd { descriptor }),
        OAuthCommitResponse::ImportLegacy | OAuthCommitResponse::ImportDevice => {
            respond(route, oauth_import_response(response, descriptor, revision));
        }
    }
}

fn descriptor_for(
    identity: &LoginIdentity,
    alias: &CredentialAlias,
    validated_identity: Option<String>,
    account_identity: Option<AccountIdentity>,
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
        label: None,
        account_identity,
        // AccountStore::add owns the first-commit timestamp for every auth
        // method. Replacement preserves the prior value, including legacy
        // None, instead of turning a re-key into a new account.
        created_at_ms: None,
    }
}

fn api_key_identity(provider: &str, secret: &[u8]) -> AccountIdentity {
    let last_four = std::str::from_utf8(secret)
        .ok()
        .map(str::trim)
        .filter(|secret| !secret.is_empty())
        .map(|secret| {
            let mut fingerprint = secret
                .chars()
                .rev()
                .filter(|character| !character.is_control())
                .take(4)
                .collect::<Vec<_>>();
            fingerprint.reverse();
            fingerprint.into_iter().collect::<String>()
        })
        .and_then(|fingerprint| AccountIdentity::sanitized_field(&fingerprint))
        .map(|fingerprint| format!("…{fingerprint}"));
    AccountIdentity {
        email: None,
        display_name: Some(last_four.map_or_else(
            || format!("{provider} API key"),
            |fingerprint| format!("{provider} API key {fingerprint}"),
        )),
        account_id: None,
        plan: None,
        issuer: None,
        captured_at: unix_ms_after(Duration::ZERO),
        verified: false,
    }
}

#[allow(clippy::too_many_arguments)]
async fn finalize_and_respond(
    store: &SqliteStoreHandle,
    accounts: &mut AccountStore<Box<dyn StoreLike>>,
    providers: &ProviderRegistry<Box<dyn ProviderRegistryStoreLike>>,
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
    // A login can flip a SEEDED-inventory provider's availability (G4b
    // decision 6 — bedrock/vertex light Available once a credential
    // exists), so the login publish carries the full provider view.
    refresh_resolver_snapshot(snapshot, accounts);
    if let Some(management) = management {
        management.publish(
            revision,
            accounts.list().to_vec(),
            providers.summaries(&provider_has_credential(accounts)),
        );
    }
    respond(route, ResponseBody::AccountLoginApi { descriptor });
}

// ─────────────────── accounts-backed provider factory ──────────────────────

/// Per-turn provider tuning derived from session metadata (G3): the
/// session's explicit effort selection and fast-mode flag, threaded from
/// `ProviderFactory::resolve_for_turn` into the provider constructors.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct ProviderTuning {
    pub effort: Option<String>,
    pub fast: bool,
    /// W-B: whether the resolved pair may declare its PROVIDER-NATIVE web
    /// tools (Anthropic server tools, OpenAI hosted search, Gemini
    /// built-ins). True on real turns; the factory clears it when the
    /// session's anthropic server tools degraded (local-fallback latch) or a
    /// daemon-owned Loom workflow requires every network effect to cross the
    /// scoped local dispatcher. Probe/validation constructions leave it false
    /// via `default()`.
    pub web_tools: bool,
}

impl ProviderTuning {
    pub(crate) fn from_metadata(metadata: &haider_protocol::session::SessionMetadataV1) -> Self {
        Self {
            effort: metadata.effort.clone(),
            fast: metadata.fast,
            web_tools: true,
        }
    }
}

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

    /// Profile-aware construction for API-family routing. Injected builders
    /// remain source-compatible and may ignore registry metadata.
    fn build_profile_descriptor(
        &self,
        profile: Option<&ProviderSummaryWire>,
        descriptor: &CredentialDescriptor,
        credential: haider_accounts::SecretHandle,
        model: &str,
    ) -> Result<Arc<dyn Provider>, HaiderError> {
        let _ = profile;
        self.build_descriptor(descriptor, credential, model)
    }

    /// Tuning-aware construction (G3): the per-turn provider carries the
    /// session's effort/fast selection. Injected builders remain source
    /// compatible and may ignore the tuning.
    fn build_tuned(
        &self,
        profile: Option<&ProviderSummaryWire>,
        descriptor: &CredentialDescriptor,
        credential: haider_accounts::SecretHandle,
        model: &str,
        tuning: &ProviderTuning,
    ) -> Result<Arc<dyn Provider>, HaiderError> {
        let _ = tuning;
        self.build_profile_descriptor(profile, descriptor, credential, model)
    }

    /// CM2 Gemini resource ownership. Injected builders remain source
    /// compatible and ignore the registry through this default.
    #[allow(clippy::too_many_arguments)]
    fn build_tuned_with_cache(
        &self,
        profile: Option<&ProviderSummaryWire>,
        descriptor: &CredentialDescriptor,
        credential: haider_accounts::SecretHandle,
        model: &str,
        tuning: &ProviderTuning,
        catalog_model: Option<&DiscoveredModel>,
        gemini_cache_registry: Arc<haider_provider::GeminiCacheRegistry>,
    ) -> Result<Arc<dyn Provider>, HaiderError> {
        let _ = catalog_model;
        let _ = gemini_cache_registry;
        self.build_tuned(profile, descriptor, credential, model, tuning)
    }
}

/// Maximum number of credential-bearing provider adapters retained by one
/// production builder. Custom endpoints and model/tuning combinations are
/// user-controlled, so this bound is also the bound on their live reqwest
/// clients and connection pools.
const ACCOUNT_PROVIDER_ADAPTER_CACHE_CAPACITY: usize = 64;

/// Adapter-effective provider profile fields. Other profile fields control
/// whether resolution is allowed or how it is displayed, but do not reach an
/// adapter constructor.
#[derive(Clone, PartialEq, Eq, Hash)]
struct AccountProviderProfileCacheKey {
    endpoint: Option<String>,
    response_open_timeout_ms: Option<u64>,
    chunk_idle_timeout_ms: Option<u64>,
    semantic_progress_timeout_ms: Option<u64>,
    openai_chat_completions: bool,
    auth_methods: Vec<AuthMethod>,
    selected_model: Option<AccountProviderModelCacheKey>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct AccountProviderModelCacheKey {
    supported_efforts: Vec<String>,
    supports_thinking_type: Option<bool>,
}

impl AccountProviderProfileCacheKey {
    fn new(profile: &ProviderSummaryWire, model: &str) -> Self {
        let selected_model = profile
            .model_details
            .iter()
            .find(|detail| detail.name == model)
            .map(|detail| AccountProviderModelCacheKey {
                supported_efforts: detail.supported_efforts.clone(),
                supports_thinking_type: detail.supports_thinking_type,
            });
        Self {
            endpoint: profile.endpoint.clone(),
            response_open_timeout_ms: profile.response_open_timeout_ms,
            chunk_idle_timeout_ms: profile.chunk_idle_timeout_ms,
            semantic_progress_timeout_ms: profile.semantic_progress_timeout_ms,
            openai_chat_completions: matches!(
                profile.api_family,
                ProviderApiFamilyWire::OpenAiChatCompletions
            ),
            auth_methods: profile.auth_methods.clone(),
            selected_model,
        }
    }
}

/// Provider-declared fields retained by the Kimi/Grok adapter. Keeping these
/// typed avoids serializing a catalog row merely to identify it.
#[derive(Clone, PartialEq, Eq, Hash)]
struct AccountProviderCatalogModelCacheKey {
    slug: String,
    display_name: String,
    context_window: Option<u64>,
    description: Option<String>,
    default_effort: Option<String>,
    supported_efforts: Vec<String>,
    visible: bool,
    priority: Option<i64>,
    extensions: Option<AccountProviderCatalogExtensionsCacheKey>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct AccountProviderCatalogExtensionsCacheKey {
    protocol: String,
    supports_reasoning: bool,
    supports_vision: bool,
    supports_tool_use: bool,
    supports_thinking_type: bool,
    supports_reasoning_effort: bool,
}

impl From<&DiscoveredModel> for AccountProviderCatalogModelCacheKey {
    fn from(model: &DiscoveredModel) -> Self {
        Self {
            slug: model.slug.clone(),
            display_name: model.display_name.clone(),
            context_window: model.context_window,
            description: model.description.clone(),
            default_effort: model.default_effort.clone(),
            supported_efforts: model.supported_efforts.clone(),
            visible: model.visible,
            priority: model.priority,
            extensions: model.extensions.as_ref().map(|extensions| {
                AccountProviderCatalogExtensionsCacheKey {
                    protocol: extensions.protocol.clone(),
                    supports_reasoning: extensions.supports_reasoning,
                    supports_vision: extensions.supports_vision,
                    supports_tool_use: extensions.supports_tool_use,
                    supports_thinking_type: extensions.supports_thinking_type,
                    supports_reasoning_effort: extensions.supports_reasoning_effort,
                }
            }),
        }
    }
}

/// Every non-secret input that can change adapter construction. Account,
/// endpoint, auth/header mode, origin policy, tuning, and catalog facts are
/// explicit; the credential fingerprint separates rotations without
/// retaining another copy of the secret. The typed profile response-open
/// timeout participates through `profile`; proxy, TLS, redirect, and DNS
/// resolver implementations remain constructor constants in one process.
#[derive(Clone, PartialEq, Eq, Hash)]
struct AccountProviderAdapterCacheKey {
    provider: String,
    alias: CredentialAlias,
    account_identity: String,
    base_url: Option<String>,
    auth_method: AuthMethod,
    credential_fingerprint: [u8; 32],
    model: String,
    tuning: ProviderTuning,
    profile: Option<AccountProviderProfileCacheKey>,
    catalog_model: Option<AccountProviderCatalogModelCacheKey>,
    gemini_cache_registry_identity: usize,
}

impl AccountProviderAdapterCacheKey {
    fn new(
        profile: Option<&ProviderSummaryWire>,
        descriptor: &CredentialDescriptor,
        credential: &haider_accounts::SecretHandle,
        model: &str,
        tuning: &ProviderTuning,
        catalog_model: Option<&DiscoveredModel>,
        gemini_cache_registry: &Arc<haider_provider::GeminiCacheRegistry>,
    ) -> Self {
        Self {
            provider: descriptor.provider.clone(),
            alias: descriptor.alias.clone(),
            account_identity: descriptor.identity.clone(),
            base_url: descriptor.base_url.clone(),
            auth_method: descriptor.auth_method,
            credential_fingerprint: *blake3::hash(credential.expose_secret()).as_bytes(),
            model: model.to_owned(),
            tuning: tuning.clone(),
            profile: profile.map(|profile| AccountProviderProfileCacheKey::new(profile, model)),
            catalog_model: if matches!(
                descriptor.provider.as_str(),
                KIMI_OAUTH_PROVIDER_NAME | GROK_OAUTH_PROVIDER_NAME
            ) {
                catalog_model
                    .filter(|catalog_model| catalog_model.slug == model)
                    .map(AccountProviderCatalogModelCacheKey::from)
            } else {
                None
            },
            gemini_cache_registry_identity: if descriptor.provider == GEMINI_PROVIDER_NAME {
                Arc::as_ptr(gemini_cache_registry) as usize
            } else {
                0
            },
        }
    }
}

struct AccountProviderAdapterCache {
    capacity: usize,
    recency: u64,
    entries: HashMap<AccountProviderAdapterCacheKey, AccountProviderAdapterCacheEntry>,
}

struct AccountProviderAdapterCacheEntry {
    adapter: Arc<dyn Provider>,
    last_used: u64,
}

impl AccountProviderAdapterCache {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            recency: 0,
            entries: HashMap::new(),
        }
    }

    fn get(&mut self, key: &AccountProviderAdapterCacheKey) -> Option<Arc<dyn Provider>> {
        let last_used = self.next_recency();
        let entry = self.entries.get_mut(key)?;
        entry.last_used = last_used;
        Some(Arc::clone(&entry.adapter))
    }

    fn insert(&mut self, key: AccountProviderAdapterCacheKey, adapter: Arc<dyn Provider>) {
        if self.capacity == 0 {
            return;
        }
        if self.entries.len() >= self.capacity
            && let Some(evicted) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key)
                .cloned()
        {
            self.entries.remove(&evicted);
        }
        let last_used = self.next_recency();
        self.entries
            .insert(key, AccountProviderAdapterCacheEntry { adapter, last_used });
    }

    fn next_recency(&mut self) -> u64 {
        self.recency = self.recency.saturating_add(1);
        self.recency
    }
}

/// Production builder for every account-backed adapter shipped in this lane.
pub(crate) struct ProductionAccountBuilder {
    adapters: StdMutex<AccountProviderAdapterCache>,
}

impl Default for ProductionAccountBuilder {
    fn default() -> Self {
        Self {
            adapters: StdMutex::new(AccountProviderAdapterCache::new(
                ACCOUNT_PROVIDER_ADAPTER_CACHE_CAPACITY,
            )),
        }
    }
}

impl ProductionAccountBuilder {
    #[cfg(test)]
    #[allow(clippy::expect_used)]
    fn with_cache_capacity(capacity: usize) -> Self {
        Self {
            adapters: StdMutex::new(AccountProviderAdapterCache::new(capacity)),
        }
    }

    #[cfg(test)]
    #[allow(clippy::expect_used)]
    fn cached_adapter_count(&self) -> Result<usize, HaiderError> {
        self.adapters
            .lock()
            .map(|adapters| adapters.entries.len())
            .map_err(|_| adapter_cache_unavailable())
    }
}

fn adapter_cache_unavailable() -> HaiderError {
    HaiderError::new(
        ErrorCode::ProviderError,
        "account provider adapter cache is unavailable",
        true,
    )
}

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
        build_account_provider(
            provider,
            None,
            None,
            AuthMethod::ApiKey,
            credential,
            model,
            alias,
            &ProviderTuning::default(),
            None,
            None,
        )
    }

    fn build_descriptor(
        &self,
        descriptor: &CredentialDescriptor,
        credential: haider_accounts::SecretHandle,
        model: &str,
    ) -> Result<Arc<dyn Provider>, HaiderError> {
        build_account_provider(
            &descriptor.provider,
            None,
            descriptor.base_url.as_deref(),
            descriptor.auth_method,
            credential,
            model,
            &descriptor.alias,
            &ProviderTuning::default(),
            None,
            None,
        )
    }

    fn build_profile_descriptor(
        &self,
        profile: Option<&ProviderSummaryWire>,
        descriptor: &CredentialDescriptor,
        credential: haider_accounts::SecretHandle,
        model: &str,
    ) -> Result<Arc<dyn Provider>, HaiderError> {
        build_account_provider(
            &descriptor.provider,
            profile,
            descriptor.base_url.as_deref(),
            descriptor.auth_method,
            credential,
            model,
            &descriptor.alias,
            &ProviderTuning::default(),
            None,
            None,
        )
    }

    fn build_tuned(
        &self,
        profile: Option<&ProviderSummaryWire>,
        descriptor: &CredentialDescriptor,
        credential: haider_accounts::SecretHandle,
        model: &str,
        tuning: &ProviderTuning,
    ) -> Result<Arc<dyn Provider>, HaiderError> {
        build_account_provider(
            &descriptor.provider,
            profile,
            descriptor.base_url.as_deref(),
            descriptor.auth_method,
            credential,
            model,
            &descriptor.alias,
            tuning,
            None,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn build_tuned_with_cache(
        &self,
        profile: Option<&ProviderSummaryWire>,
        descriptor: &CredentialDescriptor,
        credential: haider_accounts::SecretHandle,
        model: &str,
        tuning: &ProviderTuning,
        catalog_model: Option<&DiscoveredModel>,
        gemini_cache_registry: Arc<haider_provider::GeminiCacheRegistry>,
    ) -> Result<Arc<dyn Provider>, HaiderError> {
        let key = AccountProviderAdapterCacheKey::new(
            profile,
            descriptor,
            &credential,
            model,
            tuning,
            catalog_model,
            &gemini_cache_registry,
        );
        let mut adapters = self
            .adapters
            .lock()
            .map_err(|_| adapter_cache_unavailable())?;
        if let Some(adapter) = adapters.get(&key) {
            return Ok(adapter);
        }
        let adapter = build_account_provider(
            &descriptor.provider,
            profile,
            descriptor.base_url.as_deref(),
            descriptor.auth_method,
            credential,
            model,
            &descriptor.alias,
            tuning,
            catalog_model,
            Some(gemini_cache_registry),
        )?;
        adapters.insert(key, Arc::clone(&adapter));
        Ok(adapter)
    }
}

#[allow(clippy::too_many_arguments)]
fn build_account_provider(
    provider: &str,
    profile: Option<&ProviderSummaryWire>,
    base_url: Option<&str>,
    auth_method: AuthMethod,
    credential: haider_accounts::SecretHandle,
    model: &str,
    alias: &CredentialAlias,
    tuning: &ProviderTuning,
    catalog_model: Option<&DiscoveredModel>,
    gemini_cache_registry: Option<Arc<haider_provider::GeminiCacheRegistry>>,
) -> Result<Arc<dyn Provider>, HaiderError> {
    let compatible_base_url = account_openai_compatible_base_url(provider, profile, base_url);
    let anthropic_fast = anthropic_fast_for(provider, tuning, model);
    let anthropic_effort = anthropic_effort_for(tuning, model);
    let openai_effort = openai_effort_for(tuning, profile, model);
    let openai_transport = openai_transport_config(profile);
    let anthropic_transport = anthropic_transport_config(profile);
    if auth_method == AuthMethod::ApiKey
        && let Some(profile) = profile.filter(|profile| {
            matches!(profile.api_family, ProviderApiFamilyWire::AnthropicMessages)
                && !haider_provider::BUILTIN_PROVIDER_NAMES.contains(&provider)
        })
    {
        let base_url = profile.endpoint.as_deref().or(base_url).ok_or_else(|| {
            HaiderError::new(
                ErrorCode::InvalidArgument,
                format!("provider {provider} profile is missing its base_url"),
                false,
            )
        })?;
        let adapter = if profile.auth_methods.is_empty() {
            AnthropicProvider::new_custom_no_auth(credential, model, base_url)
        } else {
            AnthropicProvider::new_custom(credential, model, base_url)
        };
        return Ok(Arc::new(
            adapter
                .map_err(|error| adapter_construction_error(provider, error))?
                .with_transport_config(anthropic_transport)
                .map_err(|error| adapter_construction_error(provider, error))?
                .with_account(alias.clone())
                .with_effort(anthropic_effort.clone())
                .with_fast(anthropic_fast),
        ));
    }
    let adapter: Arc<dyn Provider> = match (provider, auth_method) {
        // W-B: FIRST-PARTY Anthropic pairs declare the server web tools;
        // Bedrock/Vertex below deliberately never take the flag (the
        // capability matrix keeps enterprise on the local client tool).
        (ANTHROPIC_PROVIDER_NAME, AuthMethod::ApiKey) => Arc::new(
            AnthropicProvider::new(credential, model)
                .map_err(|error| adapter_construction_error(provider, error))?
                .with_account(alias.clone())
                .with_effort(anthropic_effort.clone())
                .with_fast(anthropic_fast)
                .with_web_tools(tuning.web_tools),
        ),
        // G4b: Bedrock mantle — the endpoint-parameterized Anthropic
        // adapter at the profile's mantle base (LB1/LB2). Effort clamps
        // through the normalized static tables; FAST is deliberately the
        // provider-gated `anthropic_fast_for`, which refuses bedrock
        // regardless of model (decision 4).
        (BEDROCK_PROVIDER_NAME, AuthMethod::ApiKey) => {
            let endpoint = enterprise_endpoint(profile, base_url).ok_or_else(|| {
                HaiderError::new(
                    ErrorCode::InvalidArgument,
                    "bedrock profile has no configured mantle endpoint",
                    false,
                )
            })?;
            Arc::new(
                AnthropicProvider::new_endpoint(credential, model, endpoint)
                    .map_err(|error| adapter_construction_error(provider, error))?
                    .with_account(alias.clone())
                    .with_effort(anthropic_effort.clone())
                    .with_fast(anthropic_fast),
            )
        }
        // G4b: Claude on Vertex — model-in-URL, version-in-body, plain
        // Bearer (LV1). Same provider-gated fast refusal as bedrock.
        (VERTEX_PROVIDER_NAME, AuthMethod::ApiKey) => {
            let endpoint = enterprise_endpoint(profile, base_url).ok_or_else(|| {
                HaiderError::new(
                    ErrorCode::InvalidArgument,
                    "vertex profile has no configured project endpoint — set the project and location on its card",
                    false,
                )
            })?;
            Arc::new(
                AnthropicProvider::new_vertex(credential, model, endpoint)
                    .map_err(|error| adapter_construction_error(provider, error))?
                    .with_account(alias.clone())
                    .with_effort(anthropic_effort.clone())
                    .with_fast(anthropic_fast),
            )
        }
        (OPENAI_PROVIDER_NAME, AuthMethod::ApiKey) => Arc::new(
            OpenAiProvider::new(credential, model)
                .map_err(|error| adapter_construction_error(provider, error))?
                .with_transport_config(openai_transport)
                .map_err(|error| adapter_construction_error(provider, error))?
                .with_account(alias.clone())
                .with_effort(openai_effort.clone())
                // W-B: hosted web_search on the API-key Responses pair; the
                // lite request builder makes this structurally inert there.
                .with_web_search(tuning.web_tools),
        ),
        (GEMINI_PROVIDER_NAME, AuthMethod::ApiKey) => Arc::new(
            GeminiProvider::new(credential, model)
                .map_err(|error| adapter_construction_error(provider, error))?
                .with_account(alias.clone())
                .with_effort(tuning.effort.clone())
                // W-B: google_search + url_context built-ins, name-gated to
                // 3.x models inside the request builder.
                .with_web_builtins(tuning.web_tools)
                .with_cache_registry(gemini_cache_registry.unwrap_or_default()),
        ),
        (DEEPSEEK_PROVIDER_NAME, AuthMethod::ApiKey) => Arc::new(
            OpenAiCompatibleProvider::new_deepseek_api(credential, model, DEEPSEEK_BASE_URL)
                .map_err(|error| adapter_construction_error(provider, error))?
                .with_transport_config(openai_transport)
                .map_err(|error| adapter_construction_error(provider, error))?
                .with_account(alias.clone()),
        ),
        (HAIDER_CODE_PROVIDER_NAME, AuthMethod::ApiKey) => Arc::new(
            OpenAiCompatibleProvider::new_haider_code_api(credential, model, HAIDER_CODE_BASE_URL)
                .map_err(|error| adapter_construction_error(provider, error))?
                .with_transport_config(openai_transport)
                .map_err(|error| adapter_construction_error(provider, error))?
                .with_account(alias.clone()),
        ),
        (XAI_PROVIDER_NAME, AuthMethod::ApiKey) => Arc::new(
            OpenAiCompatibleProvider::new_xai_api(credential, model, XAI_BASE_URL)
                .map_err(|error| adapter_construction_error(provider, error))?
                .with_transport_config(openai_transport)
                .map_err(|error| adapter_construction_error(provider, error))?
                .with_account(alias.clone()),
        ),
        (OPENAI_COMPATIBLE_PROVIDER_NAME, AuthMethod::ApiKey) => {
            let base_url = compatible_base_url.ok_or_else(|| {
                HaiderError::new(
                    ErrorCode::InvalidArgument,
                    "openai-compatible credential is missing base_url",
                    false,
                )
            })?;
            Arc::new(
                OpenAiCompatibleProvider::new(credential, model, base_url)
                    .map_err(|error| adapter_construction_error(provider, error))?
                    .with_transport_config(openai_transport)
                    .map_err(|error| adapter_construction_error(provider, error))?
                    .with_account(alias.clone()),
            )
        }
        // G4a KEYLESS ARM: a chat-completions profile whose registry auth
        // requirement is None (auth_methods is empty on the wire summary).
        // Resolution supplies an internal construction handle when no key is
        // stored. The no-auth constructor never emits those bytes; the custom
        // adapter serves the profile origin under the TrustedLan matrix.
        (_, AuthMethod::ApiKey)
            if profile.is_some_and(|profile| {
                matches!(
                    profile.api_family,
                    ProviderApiFamilyWire::OpenAiChatCompletions
                ) && profile.auth_methods.is_empty()
            }) =>
        {
            let base_url = compatible_base_url.ok_or_else(|| {
                HaiderError::new(
                    ErrorCode::InvalidArgument,
                    format!("provider {provider} profile is missing its base_url"),
                    false,
                )
            })?;
            Arc::new(
                OpenAiCompatibleProvider::new_custom_no_auth(credential, model, base_url)
                    .map_err(|error| adapter_construction_error(provider, error))?
                    .with_transport_config(openai_transport)
                    .map_err(|error| adapter_construction_error(provider, error))?
                    .with_account(alias.clone()),
            )
        }
        // Key-requiring custom profiles (auth requirement ApiKey). The
        // empty-auth case belongs EXCLUSIVELY to the keyless arm above so
        // that arm stays load-bearing.
        (_, AuthMethod::ApiKey)
            if profile.is_some_and(|profile| {
                matches!(
                    profile.api_family,
                    ProviderApiFamilyWire::OpenAiChatCompletions
                ) && !profile.auth_methods.is_empty()
            }) =>
        {
            let base_url = compatible_base_url.ok_or_else(|| {
                HaiderError::new(
                    ErrorCode::InvalidArgument,
                    format!("provider {provider} profile is missing its base_url"),
                    false,
                )
            })?;
            Arc::new(
                custom_compatible_adapter(provider, credential, model, base_url)?
                    .with_transport_config(openai_transport)
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
                    .with_transport_config(openai_transport)
                    .map_err(|error| adapter_construction_error(provider, error))?
                    .with_account(alias.clone())
                    .with_effort(openai_effort.clone()),
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
                    .with_account(alias.clone())
                    .with_effort(anthropic_effort.clone())
                    .with_fast(anthropic_fast)
                    // W-B: OAuth server web tools ride the owner-accepted
                    // subscription risk posture (the notes carry the
                    // third-party caveat verbatim); a session-scoped 400
                    // degrade clears the flag upstream.
                    .with_web_tools(tuning.web_tools),
            )
        }
        (KIMI_OAUTH_PROVIDER_NAME, AuthMethod::OAuth) => {
            let inference = sanctioned_inference(provider).ok_or_else(|| {
                HaiderError::new(
                    ErrorCode::Unauthorized,
                    "Kimi OAuth registration is unavailable",
                    false,
                )
            })?;
            if inference.auth_mode != OAuthInferenceAuthMode::Bearer
                || inference.header_set != OAuthInferenceHeaderSet::KimiOpenAiChatCompletions
            {
                return Err(HaiderError::new(
                    ErrorCode::Unauthorized,
                    "Kimi OAuth inference metadata is invalid",
                    false,
                ));
            }
            let mut adapter = OpenAiCompatibleProvider::new_kimi_subscription(
                credential,
                model,
                inference.base_url,
            )
            .map_err(|error| adapter_construction_error(provider, error))?
            .with_transport_config(openai_transport)
            .map_err(|error| adapter_construction_error(provider, error))?
            .with_account(alias.clone())
            .with_cached_catalog_model(catalog_model);
            // G3: inject ONLY what the kimi catalog declared for this model
            // (think_efforts membership), in the shape it declared: models
            // with a thinking-type toggle take `thinking.effort`; k3-style
            // always-thinking models take top-level `reasoning_effort`.
            if let Some(effort) = &tuning.effort
                && let Some(detail) = profile.and_then(|profile| {
                    profile
                        .model_details
                        .iter()
                        .find(|detail| detail.name == model)
                })
                && detail.supported_efforts.iter().any(|level| level == effort)
            {
                adapter = if detail.supports_thinking_type == Some(true) {
                    adapter
                        .with_kimi_thinking(haider_provider::KimiThinkingConfig {
                            thinking_type: haider_provider::KimiThinkingType::Enabled,
                            effort: Some(effort.clone()),
                            keep: None,
                        })
                        .map_err(|error| adapter_construction_error(provider, error))?
                } else {
                    adapter
                        .with_kimi_reasoning_effort(effort.clone())
                        .map_err(|error| adapter_construction_error(provider, error))?
                };
            }
            Arc::new(adapter)
        }
        (GROK_OAUTH_PROVIDER_NAME, AuthMethod::OAuth) => {
            let inference = sanctioned_inference(provider).ok_or_else(|| {
                HaiderError::new(
                    ErrorCode::Unauthorized,
                    "Grok OAuth registration is unavailable",
                    false,
                )
            })?;
            if inference.auth_mode != OAuthInferenceAuthMode::Bearer
                || inference.header_set != OAuthInferenceHeaderSet::GrokOpenAiChatCompletions
            {
                return Err(HaiderError::new(
                    ErrorCode::Unauthorized,
                    "Grok OAuth inference metadata is invalid",
                    false,
                ));
            }
            Arc::new(
                OpenAiCompatibleProvider::new_grok_subscription(
                    credential,
                    model,
                    inference.base_url,
                )
                .map_err(|error| adapter_construction_error(provider, error))?
                .with_transport_config(openai_transport)
                .map_err(|error| adapter_construction_error(provider, error))?
                .with_account(alias.clone())
                .with_cached_catalog_model(catalog_model),
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

fn openai_transport_config(profile: Option<&ProviderSummaryWire>) -> OpenAiTransportConfig {
    let mut config = haider_provider::OPENAI_DEFAULT_TRANSPORT_CONFIG;
    if let Some(timeout_ms) = profile.and_then(|profile| profile.response_open_timeout_ms) {
        config.response_open_timeout = Duration::from_millis(timeout_ms);
    }
    if let Some(timeout_ms) = profile.and_then(|profile| profile.chunk_idle_timeout_ms) {
        config.chunk_idle_timeout = Duration::from_millis(timeout_ms);
    }
    if let Some(timeout_ms) = profile.and_then(|profile| profile.semantic_progress_timeout_ms) {
        config.semantic_progress_timeout = Duration::from_millis(timeout_ms);
    }
    config
}

fn anthropic_transport_config(profile: Option<&ProviderSummaryWire>) -> AnthropicTransportConfig {
    let mut transport = AnthropicProvider::transport_config();
    if let Some(timeout_ms) = profile.and_then(|profile| profile.response_open_timeout_ms) {
        transport.response_open_timeout = Duration::from_millis(timeout_ms);
    }
    if let Some(timeout_ms) = profile.and_then(|profile| profile.chunk_idle_timeout_ms) {
        transport.chunk_idle_timeout = Duration::from_millis(timeout_ms);
    }
    if let Some(timeout_ms) = profile.and_then(|profile| profile.semantic_progress_timeout_ms) {
        transport.semantic_progress_timeout = Duration::from_millis(timeout_ms);
    }
    transport
}

/// G3 fast gate at construction: fast is validated at TOGGLE time, but a
/// later model switch can leave a stale flag on a pair outside the static
/// gate — 4.7 hard-errors on `speed: "fast"` and 4.6 silently bills
/// standard, so an out-of-gate pair sends a standard request instead of a
/// request the API documents as broken for it.
///
/// G4b (decision 4, LE-x): FAST is Claude-API-only — `bedrock` and `vertex`
/// refuse REGARDLESS of model, because the effort-table normalization makes
/// `anthropic.claude-opus-5` pass the model gate while neither platform
/// serves the fast research preview.
pub(crate) fn anthropic_fast_for(provider: &str, tuning: &ProviderTuning, model: &str) -> bool {
    !matches!(provider, BEDROCK_PROVIDER_NAME | VERTEX_PROVIDER_NAME)
        && tuning.fast
        && haider_provider::anthropic_fast_mode_supported(model)
}

/// The endpoint an enterprise (anthropic-family) profile serves from: the
/// PROFILE origin is the authority (the card writes it there), with the
/// credential descriptor's `base_url` as the compatibility fallback.
fn enterprise_endpoint<'a>(
    profile: Option<&'a ProviderSummaryWire>,
    credential_base_url: Option<&'a str>,
) -> Option<&'a str> {
    profile
        .and_then(|profile| profile.endpoint.as_deref())
        .or(credential_base_url)
}

/// G3 effort gate at construction (review of record): effort is validated at
/// SELECTION time against the then-current pair, but a later model switch
/// can leave a stale level outside the new pair's ladder. Anthropic clamps
/// down the documented ladder (Claude Code's published fallback rule) so a
/// stale `xhigh` on a 4.6 row sends `high`, never a documented-400 request;
/// a model with no documented ladder passes the selection through verbatim.
pub(crate) fn anthropic_effort_for(tuning: &ProviderTuning, model: &str) -> Option<String> {
    haider_provider::anthropic_effort_clamp(model, tuning.effort.as_deref())
}

/// Same stale-pair gate for OpenAI pairs, sourced from the pair's CATALOG
/// ladder (the kimi-arm pattern): a declared ladder that excludes the stale
/// level drops it to `None` (provider default — vocabularies differ across
/// families, so no cross-family fallback order is invented); a pair whose
/// catalog declares NO ladder passes the selection through verbatim.
pub(crate) fn openai_effort_for(
    tuning: &ProviderTuning,
    profile: Option<&ProviderSummaryWire>,
    model: &str,
) -> Option<String> {
    let effort = tuning.effort.clone()?;
    let declared = profile.and_then(|profile| {
        profile
            .model_details
            .iter()
            .find(|detail| detail.name == model)
            .map(|detail| detail.supported_efforts.as_slice())
    });
    match declared {
        Some(ladder) if !ladder.is_empty() && !ladder.contains(&effort) => None,
        _ => Some(effort),
    }
}

fn account_openai_compatible_base_url<'a>(
    provider: &str,
    profile: Option<&'a ProviderSummaryWire>,
    credential_base_url: Option<&'a str>,
) -> Option<&'a str> {
    if provider == OPENAI_COMPATIBLE_PROVIDER_NAME {
        return credential_base_url;
    }
    profile
        .filter(|profile| {
            matches!(
                profile.api_family,
                ProviderApiFamilyWire::OpenAiChatCompletions
            )
        })
        .and_then(|profile| profile.endpoint.as_deref().or(credential_base_url))
}

/// Which compatible constructor a (provider, origin) pair routes through —
/// the ONE mapping behind [`custom_compatible_adapter`], split out so the
/// azure/builtin/custom routing is unit-pinned (G4b).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompatibleAdapterRoute {
    /// Azure OpenAI v1 (G4b): `api-key` header under the Strict fence.
    Azure,
    /// A builtin id that ever routes here (defensive — none does today)
    /// keeps the Strict fence and the Bearer header.
    Builtin,
    /// Custom provenance: TrustedLan matrix, Bearer header (G4a).
    Custom,
}

pub(crate) fn compatible_adapter_route(provider: &str, base_url: &str) -> CompatibleAdapterRoute {
    if azure_openai_origin(base_url) {
        CompatibleAdapterRoute::Azure
    } else if haider_provider::BUILTIN_PROVIDER_NAMES.contains(&provider) {
        CompatibleAdapterRoute::Builtin
    } else {
        CompatibleAdapterRoute::Custom
    }
}

/// Builds the profile-routed compatible adapter under the provenance-correct
/// origin policy (G4a) and header mode (G4b): azure origins take the
/// `api-key` adapter, custom provider ids the TrustedLan matrix, and a
/// builtin id that ever routes here (defensive — none does today) keeps the
/// Strict fence.
fn custom_compatible_adapter(
    provider: &str,
    credential: haider_accounts::SecretHandle,
    model: &str,
    base_url: &str,
) -> Result<OpenAiCompatibleProvider, HaiderError> {
    match compatible_adapter_route(provider, base_url) {
        CompatibleAdapterRoute::Azure => {
            OpenAiCompatibleProvider::new_azure(credential, model, base_url)
        }
        CompatibleAdapterRoute::Builtin => {
            OpenAiCompatibleProvider::new(credential, model, base_url)
        }
        CompatibleAdapterRoute::Custom => {
            OpenAiCompatibleProvider::new_custom(credential, model, base_url)
        }
    }
    .map_err(|error| adapter_construction_error(provider, error))
}

/// Internal construction token for auth-None profiles (G4a). Provider
/// adapters require a `SecretHandle` by type, but their no-auth constructors
/// never place these bytes in a request header or body.
pub(crate) const KEYLESS_CONSTRUCTION_TOKEN: &[u8] = b"no-auth";

/// Mints the placeholder credential through the same vault machinery every
/// real secret uses, so the handle's redaction/zeroization laws hold.
fn keyless_construction_credential() -> Result<haider_accounts::SecretHandle, HaiderError> {
    let staging = MemoryVault::default();
    let alias = CredentialAlias::new("keyless-placeholder");
    staging
        .put(&alias, KEYLESS_CONSTRUCTION_TOKEN)
        .and_then(|()| staging.resolve(&alias))
        .map_err(|error| {
            HaiderError::new(
                ErrorCode::Internal,
                format!(
                    "could not stage the keyless construction credential: {}",
                    error.message
                ),
                false,
            )
        })
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

fn provider_tuning_with_web_degrade(
    metadata: &haider_protocol::session::SessionMetadataV1,
    web_degrade: crate::worker::WebCapabilityDegrade,
) -> ProviderTuning {
    let mut tuning = ProviderTuning::from_metadata(metadata);
    if web_degrade.disable_hosted_web_tools
        || (web_degrade.anthropic_web_tools
            && matches!(
                metadata.provider.as_str(),
                ANTHROPIC_PROVIDER_NAME | ANTHROPIC_OAUTH_PROVIDER_NAME
            ))
    {
        tuning.web_tools = false;
    }
    tuning
}

#[derive(Clone)]
pub(crate) struct AccountsProviderFactory {
    snapshot: AccountsSnapshot,
    management: Option<ManagementSnapshot>,
    model_source: Option<Arc<CachedProviderModelSource>>,
    vault: VaultProvision,
    builder: Arc<dyn AccountProviderBuilder>,
    broker: Option<CredentialBroker>,
    gemini_cache_registry: Arc<haider_provider::GeminiCacheRegistry>,
    resilience: AccountsResilienceConfig,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct AccountsResilienceConfig {
    pub fallback_chain: Vec<ProviderTargetV1>,
    pub promotion_targets: HashMap<String, ProviderTargetV1>,
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
            management: None,
            model_source: None,
            vault,
            builder,
            broker: None,
            gemini_cache_registry: Arc::default(),
            resilience: AccountsResilienceConfig::default(),
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
            management: None,
            model_source: None,
            vault,
            builder,
            broker: Some(broker),
            gemini_cache_registry: Arc::default(),
            resilience: AccountsResilienceConfig::default(),
        }
    }

    pub(crate) fn new_with_management(
        snapshot: AccountsSnapshot,
        management: ManagementSnapshot,
        vault: VaultProvision,
        builder: Arc<dyn AccountProviderBuilder>,
    ) -> Self {
        Self {
            snapshot,
            management: Some(management),
            model_source: None,
            vault,
            builder,
            broker: None,
            gemini_cache_registry: Arc::default(),
            resilience: AccountsResilienceConfig::default(),
        }
    }

    pub(crate) fn with_broker_and_management(
        snapshot: AccountsSnapshot,
        management: ManagementSnapshot,
        vault: VaultProvision,
        builder: Arc<dyn AccountProviderBuilder>,
        broker: CredentialBroker,
    ) -> Self {
        Self {
            snapshot,
            management: Some(management),
            model_source: None,
            vault,
            builder,
            broker: Some(broker),
            gemini_cache_registry: Arc::default(),
            resilience: AccountsResilienceConfig::default(),
        }
    }

    pub(crate) fn with_resilience(mut self, resilience: AccountsResilienceConfig) -> Self {
        self.resilience = resilience;
        self
    }

    /// Shares the daemon's typed durable-catalog projection with per-turn
    /// adapters. This is capability metadata only: the session's pinned
    /// model remains authoritative even when no matching record exists.
    pub(crate) fn with_model_source(
        mut self,
        model_source: Arc<CachedProviderModelSource>,
    ) -> Self {
        self.model_source = Some(model_source);
        self
    }

    fn provider_profile(&self, provider: &str) -> Option<ProviderSummaryWire> {
        self.management
            .as_ref()?
            .read()?
            .providers
            .into_iter()
            .find(|profile| profile.provider == provider)
    }

    fn model_context_window(&self, provider: &str, model: &str) -> Option<u64> {
        self.provider_profile(provider)?
            .model_details
            .into_iter()
            .find(|detail| detail.name == model)
            .and_then(|detail| detail.context_window)
    }

    #[cfg(test)]
    async fn resolve_compaction_promotion(
        &self,
        metadata: &haider_protocol::session::SessionMetadataV1,
    ) -> Option<haider_core::ProviderPairSwitchTarget> {
        self.resolve_compaction_promotion_with_web(
            metadata,
            crate::worker::WebCapabilityDegrade::default(),
        )
        .await
    }

    async fn resolve_compaction_promotion_with_web(
        &self,
        metadata: &haider_protocol::session::SessionMetadataV1,
        web_degrade: crate::worker::WebCapabilityDegrade,
    ) -> Option<haider_core::ProviderPairSwitchTarget> {
        let target = self.resilience.promotion_targets.get(&metadata.provider)?;
        if target.provider != metadata.provider {
            return None;
        }
        let target_profile = self.provider_profile(&target.provider)?;
        if !target_profile.enabled
            || !matches!(
                target_profile.availability,
                ProviderAvailabilityWire::Available
            )
        {
            return None;
        }
        let model = target.model.as_ref()?.clone();
        if model == metadata.model {
            return None;
        }
        let current_window = self.model_context_window(&metadata.provider, &metadata.model)?;
        let target_window = self.model_context_window(&target.provider, &model)?;
        if target_window <= current_window {
            return None;
        }
        let has_signed_in_credential = self.snapshot.lock().ok().is_some_and(|rows| {
            rows.iter()
                .any(|row| row.provider == target.provider && row.active)
        });
        if !has_signed_in_credential {
            return None;
        }
        let resolved = self.resolve_account(&target.provider, None).await.ok()?;
        let credential = self.resolve_secret(&resolved.descriptor).await.ok()?;
        let oauth_access_fingerprint = (matches!(
            resolved.descriptor.provider.as_str(),
            KIMI_OAUTH_PROVIDER_NAME | OPENAI_OAUTH_PROVIDER_NAME | GROK_OAUTH_PROVIDER_NAME
        ) && resolved.descriptor.auth_method == AuthMethod::OAuth)
            .then(|| *blake3::hash(credential.expose_secret()).as_bytes());
        let mut target_metadata = metadata.clone();
        target_metadata.model = model.clone();
        let tuning = provider_tuning_with_web_degrade(&target_metadata, web_degrade);
        let provider = self
            .build_provider(&resolved.descriptor, credential, &target_metadata, &tuning)
            .ok()?;
        let capabilities = provider.capabilities().await;
        let provider_request_state = crate::worker::provider_derived_request_state(
            &target.provider,
            &capabilities,
            web_degrade,
        );
        let auth_scope = match provider.credential_surface() {
            haider_provider::ProviderCredentialSurface::Opaque => "opaque",
            haider_provider::ProviderCredentialSurface::ApiKey => "api_key",
            haider_provider::ProviderCredentialSurface::OAuthSubscriptionBearer => {
                "oauth_subscription"
            }
            haider_provider::ProviderCredentialSurface::CloudBearer => "cloud_bearer",
        }
        .to_owned();
        let next_resolver = AccountsAttemptResolver::new(
            self.clone(),
            target_metadata,
            tuning,
            oauth_access_fingerprint,
            false,
        )
        .with_web_degrade(web_degrade);
        Some(haider_core::ProviderPairSwitchTarget {
            provider,
            account: resolved.descriptor.alias,
            provider_name: target.provider.clone(),
            model,
            context_window: Some(target_window),
            cached_input_is_subset: crate::worker::cached_input_is_subset_for_provider(
                &target.provider,
            ),
            provider_request_state,
            auth_scope,
            attempt_resolver: Some(Arc::new(next_resolver)),
            cause: haider_core::ProviderPairSwitchCause::CompactionGuard,
        })
    }

    /// Builds the adapter under an EXPLICIT tuning. W-B threads the tuning
    /// in rather than deriving it here, so one turn's web-capability degrade
    /// reaches every construction on that turn — the first attempt, an
    /// auth-refresh retry, and a rotation alike.
    fn build_provider(
        &self,
        descriptor: &CredentialDescriptor,
        credential: haider_accounts::SecretHandle,
        metadata: &haider_protocol::session::SessionMetadataV1,
        tuning: &ProviderTuning,
    ) -> Result<Arc<dyn Provider>, HaiderError> {
        let profile = self.provider_profile(&descriptor.provider);
        let catalog_model = self
            .model_source
            .as_ref()
            .and_then(|source| source.models(&descriptor.provider))
            .and_then(|models| {
                models
                    .into_iter()
                    .find(|model| model.slug == metadata.model)
            });
        let provider = self.builder.build_tuned_with_cache(
            profile.as_ref(),
            descriptor,
            credential,
            &metadata.model,
            tuning,
            catalog_model.as_ref(),
            Arc::clone(&self.gemini_cache_registry),
        )?;
        if descriptor.provider == OPENAI_OAUTH_PROVIDER_NAME {
            let summary = descriptor.account_identity.as_ref().map_or_else(
                || {
                    let legacy = descriptor
                        .identity
                        .chars()
                        .filter(|character| !character.is_control())
                        .take(512)
                        .collect::<String>();
                    let legacy = legacy.trim();
                    if legacy.is_empty() {
                        descriptor.alias.as_str().to_owned()
                    } else {
                        legacy.to_owned()
                    }
                },
                AccountIdentity::summary,
            );
            Ok(Arc::new(IdentityAnnotatedProvider {
                inner: provider,
                summary,
            }))
        } else {
            Ok(provider)
        }
    }

    /// G4a keyless resolution: an ENABLED custom chat-completions profile
    /// whose auth requirement is None (empty `auth_methods` on the wire
    /// summary) serves turns WITHOUT any stored credential — a synthesized
    /// descriptor at the profile origin plus a headerless construction
    /// handle. The
    /// provider id is also the synthetic account alias: custom-provider
    /// identity is the cache account scope even when no credential exists,
    /// so the v3 prompt-cache cohort has the same isolation inputs as a keyed
    /// custom provider. This runs only after account resolution reported
    /// `CredentialMissing`, so a stored key always wins.
    fn keyless_account(
        &self,
        provider: &str,
    ) -> Option<(ResolvedAccount, haider_accounts::SecretHandle)> {
        if haider_provider::BUILTIN_PROVIDER_NAMES.contains(&provider) {
            return None;
        }
        let profile = self.provider_profile(provider)?;
        if !matches!(
            profile.api_family,
            ProviderApiFamilyWire::OpenAiChatCompletions | ProviderApiFamilyWire::AnthropicMessages
        ) || !profile.auth_methods.is_empty()
            || !profile.enabled
        {
            return None;
        }
        let base_url = profile.endpoint.clone()?;
        let descriptor = CredentialDescriptor {
            alias: CredentialAlias::new(provider),
            provider: provider.to_owned(),
            base_url: Some(base_url),
            auth_method: AuthMethod::ApiKey,
            identity: "keyless local endpoint".to_owned(),
            status: CredentialStatus::Ok,
            active: true,
            label: None,
            account_identity: None,
            created_at_ms: None,
        };
        let credential = keyless_construction_credential().ok()?;
        Some((
            ResolvedAccount {
                descriptor,
                rotation: None,
            },
            credential,
        ))
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
                CredentialStatus::Expired
                | CredentialStatus::Revoked
                | CredentialStatus::Ok
                | CredentialStatus::NeedsAttention { .. } => RotationCause::Error,
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
        tuning: &ProviderTuning,
    ) -> Result<(ResolvedAccount, Arc<dyn Provider>, Option<[u8; 32]>, bool), HaiderError> {
        let mut resolved = match self.resolve_account(&metadata.provider, None).await {
            Ok(resolved) => resolved,
            // G4a: no credential AT ALL for an auth-None custom profile is
            // the keyless case, not an error. Any other failure — including
            // CredentialMissing for a provider that DOES require auth —
            // propagates unchanged.
            Err(error) if error.code == ErrorCode::CredentialMissing => {
                let Some((resolved, credential)) = self.keyless_account(&metadata.provider) else {
                    return Err(error);
                };
                let provider =
                    self.build_provider(&resolved.descriptor, credential, metadata, tuning)?;
                return Ok((resolved, provider, None, true));
            }
            Err(error) => return Err(error),
        };
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
        let oauth_access_fingerprint = (matches!(
            resolved.descriptor.provider.as_str(),
            KIMI_OAUTH_PROVIDER_NAME | OPENAI_OAUTH_PROVIDER_NAME | GROK_OAUTH_PROVIDER_NAME
        ) && resolved.descriptor.auth_method == AuthMethod::OAuth)
            .then(|| *blake3::hash(credential.expose_secret()).as_bytes());
        let provider = self.build_provider(&resolved.descriptor, credential, metadata, tuning)?;
        Ok((resolved, provider, oauth_access_fingerprint, false))
    }
}

/// Decorates only the operator-facing ChatGPT model-entitlement rejection.
/// The summary is public descriptor metadata; token bytes never enter this
/// adapter or its error stream.
struct IdentityAnnotatedProvider {
    inner: Arc<dyn Provider>,
    summary: String,
}

impl std::fmt::Debug for IdentityAnnotatedProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IdentityAnnotatedProvider")
            .field("summary", &self.summary)
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl Provider for IdentityAnnotatedProvider {
    fn trusts_default_route_absence(&self) -> bool {
        self.inner.trusts_default_route_absence()
    }

    fn credential_surface(&self) -> haider_provider::ProviderCredentialSurface {
        self.inner.credential_surface()
    }

    fn usage_lane_dimensions(&self) -> haider_protocol::provider::UsageLaneDimensions {
        self.inner.usage_lane_dimensions()
    }

    fn rendered_cache_prefix_digests(
        &self,
        request: &TurnRequest,
    ) -> Option<haider_protocol::provider::PrefixDigests> {
        self.inner.rendered_cache_prefix_digests(request)
    }

    fn prepare_turn(&self, request: &TurnRequest) -> Option<PreparedTurn> {
        self.inner.prepare_turn(request)
    }

    fn prepare_turn_with_tools(
        &self,
        request: &TurnRequest,
        tools: &[ToolDefinition],
    ) -> Option<PreparedTurn> {
        self.inner.prepare_turn_with_tools(request, tools)
    }

    async fn prewarm(&self) {
        self.inner.prewarm().await;
    }

    async fn capabilities(&self) -> haider_protocol::provider::CapabilityDoc {
        self.inner.capabilities().await
    }

    async fn stream_turn(&self, request: TurnRequest) -> Result<ProviderStream, ProviderError> {
        annotate_provider_stream(self.inner.stream_turn(request).await, &self.summary)
    }

    async fn stream_prepared_turn(
        &self,
        request: TurnRequest,
        prepared: Option<PreparedTurn>,
    ) -> Result<ProviderStream, ProviderError> {
        annotate_provider_stream(
            self.inner.stream_prepared_turn(request, prepared).await,
            &self.summary,
        )
    }

    async fn stream_prepared_turn_ref(
        &self,
        request: &TurnRequest,
        prepared: Option<PreparedTurn>,
    ) -> Result<ProviderStream, ProviderError> {
        annotate_provider_stream(
            self.inner.stream_prepared_turn_ref(request, prepared).await,
            &self.summary,
        )
    }
}

fn annotate_provider_stream(
    result: Result<ProviderStream, ProviderError>,
    summary: &str,
) -> Result<ProviderStream, ProviderError> {
    let mut stream = result.map_err(|error| annotate_model_rejection(error, summary))?;
    let summary = summary.to_owned();
    let (sender, receiver) = mpsc::channel(32);
    let producer = tokio::spawn(async move {
        while let Some(item) = stream.recv().await {
            let item = item.map_err(|error| annotate_model_rejection(error, &summary));
            if sender.send(item).await.is_err() {
                break;
            }
        }
    });
    Ok(ProviderStream::owned(receiver, producer))
}

fn annotate_model_rejection(mut error: ProviderError, summary: &str) -> ProviderError {
    if error.message.contains("ChatGPT account") && !error.message.contains("account used:") {
        error.message = format!("{} (account used: {summary})", error.message);
    }
    error
}

struct AccountsAttemptResolver {
    factory: AccountsProviderFactory,
    metadata: haider_protocol::session::SessionMetadataV1,
    /// Trust is frozen with the turn exactly like the advertised tool pack.
    /// A cross-provider fallback cannot safely change this class mid-turn:
    /// core would otherwise retain the previous pair's capability pack.
    lockdown: bool,
    /// The tuning the turn RESOLVED with (W-B: including whether this pair
    /// may declare its provider-native web tools). A mid-turn rotation or
    /// auth-refresh rebuild must not silently re-enable a degraded
    /// capability, so the resolver replays this tuning verbatim.
    tuning: ProviderTuning,
    auth_refresh_attempted: AtomicBool,
    web_fallback_attempted: AtomicBool,
    oauth_access_fingerprint: Option<[u8; 32]>,
    web_degrade: crate::worker::WebCapabilityDegrade,
    /// Index of the chain entry that produced this lane. `None` means the
    /// turn started from session metadata and must locate that pair once.
    /// Carrying the cursor across hops makes traversal strictly no-wrap even
    /// when a provider occurs more than once with different models.
    fallback_cursor: Option<usize>,
}

impl AccountsAttemptResolver {
    fn new(
        factory: AccountsProviderFactory,
        metadata: haider_protocol::session::SessionMetadataV1,
        tuning: ProviderTuning,
        oauth_access_fingerprint: Option<[u8; 32]>,
        active_no_auth: bool,
    ) -> Self {
        let profile = factory.provider_profile(&metadata.provider);
        let lockdown = if active_no_auth {
            crate::auto_hermetic::provider_policy_for_active(profile.as_ref(), true).is_lockdown()
        } else {
            profile.as_ref().is_some_and(|profile| {
                crate::auto_hermetic::provider_policy_for_active(Some(profile), false).is_lockdown()
            })
        };
        Self {
            factory,
            metadata,
            lockdown,
            tuning,
            auth_refresh_attempted: AtomicBool::new(false),
            web_fallback_attempted: AtomicBool::new(false),
            oauth_access_fingerprint,
            web_degrade: crate::worker::WebCapabilityDegrade::default(),
            fallback_cursor: None,
        }
    }

    fn with_web_degrade(mut self, web_degrade: crate::worker::WebCapabilityDegrade) -> Self {
        self.web_degrade = web_degrade;
        self
    }

    fn at_fallback_cursor(mut self, cursor: usize) -> Self {
        self.fallback_cursor = Some(cursor);
        self
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
        if error.presentation.subcode.as_str() == "provider-web-tool-rejected"
            && self.tuning.web_tools
            && matches!(
                self.metadata.provider.as_str(),
                haider_provider::ANTHROPIC_PROVIDER_NAME
                    | haider_provider::ANTHROPIC_OAUTH_PROVIDER_NAME
            )
            && !self.web_fallback_attempted.swap(true, Ordering::AcqRel)
        {
            let Some(current) = self.current_descriptor(current_account) else {
                return Ok(haider_core::ProviderAttemptDecision::Stop);
            };
            let credential = self.factory.resolve_secret(&current).await?;
            let mut tuning = self.tuning.clone();
            tuning.web_tools = false;
            let provider =
                self.factory
                    .build_provider(&current, credential, &self.metadata, &tuning)?;
            return Ok(haider_core::ProviderAttemptDecision::Fallback {
                provider,
                account: current.alias,
            });
        }
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
                // G4b (LV2): the vertex gcloud-refresh credential recovers
                // in-turn exactly like OAuth — the broker re-mints its
                // token through the mocked/production shell-out once.
                if (current.auth_method == AuthMethod::OAuth
                    || crate::gcloud::is_gcloud_refresh_descriptor(&current))
                    && !self.auth_refresh_attempted.swap(true, Ordering::AcqRel)
                {
                    let Some(broker) = &self.factory.broker else {
                        return Ok(haider_core::ProviderAttemptDecision::Stop);
                    };
                    match broker
                        .refresh_after_auth_failure(&current, self.oauth_access_fingerprint)
                        .await
                    {
                        Ok(credential) => {
                            let provider = self.factory.build_provider(
                                &current,
                                credential,
                                &self.metadata,
                                &self.tuning,
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
            | ProviderErrorKind::ContextExceeded
            | ProviderErrorKind::InvalidRequest
            | ProviderErrorKind::NetworkUnavailable
            | ProviderErrorKind::Transport
            | ProviderErrorKind::MalformedFrame
            | ProviderErrorKind::InvalidUtf8
            | ProviderErrorKind::Internal
            | ProviderErrorKind::QuotaExhausted
            | ProviderErrorKind::StreamInterrupted
            | ProviderErrorKind::ConnectionConfiguration => {
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
        let provider = self.factory.build_provider(
            &resolved.descriptor,
            credential,
            &self.metadata,
            &self.tuning,
        )?;
        Ok(haider_core::ProviderAttemptDecision::Rotate(
            haider_core::ResolvedProviderAttempt {
                provider,
                account: resolved.descriptor.alias,
                rotation,
            },
        ))
    }

    async fn resolve_fallback(
        &self,
        _current_account: &CredentialAlias,
        error: &haider_provider::ProviderError,
    ) -> Result<haider_core::ProviderAttemptDecision, HaiderError> {
        if !matches!(
            error.kind,
            ProviderErrorKind::Authentication
                | ProviderErrorKind::RateLimited
                | ProviderErrorKind::Overloaded
                | ProviderErrorKind::QuotaExhausted
        ) {
            return Ok(haider_core::ProviderAttemptDecision::Stop);
        }
        // The provider identity is part of a lockdown turn's frozen
        // envelope (notably its one writable sandbox). Crossing providers
        // would either lift the ceiling or write into the wrong sandbox.
        if self.lockdown {
            return Ok(haider_core::ProviderAttemptDecision::Stop);
        }
        let chain = &self.factory.resilience.fallback_chain;
        let start = self.fallback_cursor.map_or_else(
            || {
                chain
                    .iter()
                    .position(|entry| entry.provider == self.metadata.provider)
            },
            Some,
        );
        let Some(start) = start else {
            return Ok(haider_core::ProviderAttemptDecision::Stop);
        };

        for (index, entry) in chain.iter().enumerate().skip(start.saturating_add(1)) {
            if entry.provider == self.metadata.provider {
                continue;
            }
            let target_profile = self.factory.provider_profile(&entry.provider);
            if target_profile.as_ref().is_some_and(|profile| {
                !profile.enabled
                    || profile.trust != haider_rpc::ProviderTrustWire::Full
                    || !matches!(profile.availability, ProviderAvailabilityWire::Available)
            }) {
                continue;
            }
            let has_signed_in_credential = self.factory.snapshot.lock().ok().is_some_and(|rows| {
                rows.iter()
                    .any(|row| row.provider == entry.provider && row.active)
            });
            if !has_signed_in_credential {
                continue;
            }
            let model = match entry
                .model
                .clone()
                .or_else(|| target_profile.and_then(|profile| profile.default_model))
            {
                Some(model) if !model.trim().is_empty() => model,
                _ => continue,
            };
            let resolved = match self.factory.resolve_account(&entry.provider, None).await {
                Ok(resolved) => resolved,
                Err(_) => continue,
            };
            let credential = match self.factory.resolve_secret(&resolved.descriptor).await {
                Ok(credential) => credential,
                Err(_) => continue,
            };
            let oauth_access_fingerprint = (matches!(
                resolved.descriptor.provider.as_str(),
                KIMI_OAUTH_PROVIDER_NAME | OPENAI_OAUTH_PROVIDER_NAME | GROK_OAUTH_PROVIDER_NAME
            ) && resolved.descriptor.auth_method
                == AuthMethod::OAuth)
                .then(|| *blake3::hash(credential.expose_secret()).as_bytes());
            let mut metadata = self.metadata.clone();
            metadata.provider = entry.provider.clone();
            metadata.model = model.clone();
            let tuning = provider_tuning_with_web_degrade(&metadata, self.web_degrade);
            let provider = match self.factory.build_provider(
                &resolved.descriptor,
                credential,
                &metadata,
                &tuning,
            ) {
                Ok(provider) => provider,
                Err(_) => continue,
            };
            let capabilities = provider.capabilities().await;
            let provider_request_state = crate::worker::provider_derived_request_state(
                &entry.provider,
                &capabilities,
                self.web_degrade,
            );
            let auth_scope = match provider.credential_surface() {
                haider_provider::ProviderCredentialSurface::Opaque => "opaque",
                haider_provider::ProviderCredentialSurface::ApiKey => "api_key",
                haider_provider::ProviderCredentialSurface::OAuthSubscriptionBearer => {
                    "oauth_subscription"
                }
                haider_provider::ProviderCredentialSurface::CloudBearer => "cloud_bearer",
            }
            .to_owned();
            let context_window = self.factory.model_context_window(&entry.provider, &model);
            let next_resolver = AccountsAttemptResolver::new(
                self.factory.clone(),
                metadata,
                tuning,
                oauth_access_fingerprint,
                false,
            )
            .with_web_degrade(self.web_degrade)
            .at_fallback_cursor(index);
            return Ok(haider_core::ProviderAttemptDecision::Switch(
                haider_core::ProviderPairSwitchTarget {
                    provider,
                    account: resolved.descriptor.alias,
                    provider_name: entry.provider.clone(),
                    model,
                    context_window,
                    cached_input_is_subset: crate::worker::cached_input_is_subset_for_provider(
                        &entry.provider,
                    ),
                    provider_request_state,
                    auth_scope,
                    attempt_resolver: Some(Arc::new(next_resolver)),
                    cause: haider_core::ProviderPairSwitchCause::FallbackChain,
                },
            ));
        }
        Ok(haider_core::ProviderAttemptDecision::Stop)
    }
}

impl AccountsProviderFactory {
    /// The one resolution body. `tuning` is the turn's explicit tuning —
    /// W-B's web-capability degrade is expressed there and nowhere else, so
    /// the native declaration, the auth-refresh rebuild, and the rotation
    /// rebuild all agree by construction.
    async fn resolve_turn_tuned(
        &self,
        metadata: &haider_protocol::session::SessionMetadataV1,
        tuning: ProviderTuning,
        web_degrade: crate::worker::WebCapabilityDegrade,
    ) -> Result<crate::worker::ResolvedTurnProvider, HaiderError> {
        let (resolved, provider, oauth_access_fingerprint, active_no_auth) =
            self.resolve_provider(metadata, &tuning).await?;
        let rotation_budget_consumed = resolved.rotation.is_some();
        let context_window = self.model_context_window(&metadata.provider, &metadata.model);
        let compaction_promotion = self
            .resolve_compaction_promotion_with_web(metadata, web_degrade)
            .await;
        Ok(crate::worker::ResolvedTurnProvider {
            provider,
            provider_name: metadata.provider.clone(),
            model: metadata.model.clone(),
            context_window,
            account_alias: Some(resolved.descriptor.alias.as_str().to_owned()),
            active_no_auth,
            initial_rotation: resolved.rotation,
            rotation_budget_consumed,
            attempt_resolver: self.broker.as_ref().map(|_| {
                Arc::new(
                    AccountsAttemptResolver::new(
                        self.clone(),
                        metadata.clone(),
                        tuning.clone(),
                        oauth_access_fingerprint,
                        active_no_auth,
                    )
                    .with_web_degrade(web_degrade),
                ) as Arc<dyn haider_core::ProviderAttemptResolver>
            }),
            compaction_promotion,
        })
    }
}

#[async_trait::async_trait]
impl crate::worker::ProviderFactory for AccountsProviderFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &haider_protocol::session::SessionMetadataV1,
    ) -> Result<crate::worker::ResolvedTurnProvider, HaiderError> {
        let web_degrade = crate::worker::WebCapabilityDegrade::default();
        self.resolve_turn_tuned(
            metadata,
            provider_tuning_with_web_degrade(metadata, web_degrade),
            web_degrade,
        )
        .await
    }

    /// W-B (decision 1, "local fallback on refusal"): once this session's
    /// Anthropic SERVER web tools have 400ed, the next turn on an Anthropic
    /// pair is built with `web_tools` CLEARED — no declaration, and the
    /// advertisement seam hands the model the local `web_fetch` instead. The
    /// latch is Anthropic-specific: a session that switches to another pair
    /// still gets that pair's own native web capability.
    async fn resolve_for_turn_with_web(
        &self,
        metadata: &haider_protocol::session::SessionMetadataV1,
        degrade: crate::worker::WebCapabilityDegrade,
    ) -> Result<crate::worker::ResolvedTurnProvider, HaiderError> {
        self.resolve_turn_tuned(
            metadata,
            provider_tuning_with_web_degrade(metadata, degrade),
            degrade,
        )
        .await
    }

    async fn reconcile_cache_scope(
        &self,
        session_id: &haider_protocol::ids::SessionId,
        provider: &str,
    ) {
        if provider != haider_provider::GEMINI_PROVIDER_NAME {
            let _ = self
                .gemini_cache_registry
                .delete_scope(session_id.as_str())
                .await;
        }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DurableAccountMutation {
    Add,
    Remove,
}

#[derive(Debug, PartialEq, Eq)]
struct DurableAccountHead {
    command_id: String,
    revision: u64,
    mutation: DurableAccountMutation,
}

struct DurableAccountReceiptRows {
    login: Vec<LoginReceiptRow>,
    oauth_add: Vec<AccountAddReceiptRow>,
    remove: Vec<ManagementReceiptRow>,
}

impl DurableAccountReceiptRows {
    async fn load(store: &SqliteStoreHandle) -> Result<Self, HaiderError> {
        Ok(Self {
            login: store.login_receipts().await?,
            oauth_add: store.account_add_receipts().await?,
            remove: store
                .management_receipts(ACCOUNT_REMOVE_METHOD.to_owned())
                .await?,
        })
    }
}

fn record_durable_account_head(
    heads: &mut HashMap<String, DurableAccountHead>,
    alias: String,
    candidate: DurableAccountHead,
) -> Result<(), HaiderError> {
    match heads.entry(alias) {
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(candidate);
        }
        std::collections::hash_map::Entry::Occupied(mut entry) => {
            let current = entry.get();
            if candidate.revision == current.revision
                && (candidate.command_id != current.command_id
                    || candidate.mutation != current.mutation)
            {
                return Err(HaiderError::new(
                    ErrorCode::StoreCorrupt,
                    "two account mutations share one management revision",
                    false,
                ));
            }
            if candidate.revision > current.revision {
                entry.insert(candidate);
            }
        }
    }
    Ok(())
}

/// Reconstructs the durable account lifecycle head for every alias.
///
/// Committed login/account.add receipts are durable adds; a committed
/// account.remove receipt is the corresponding durable tombstone. Receipts
/// remain replayable, so startup must select by the revision allocated in the
/// same transaction as each commit instead of treating every historical add
/// as current state.
async fn durable_account_heads(
    store: &SqliteStoreHandle,
) -> Result<HashMap<String, DurableAccountHead>, HaiderError> {
    let rows = DurableAccountReceiptRows::load(store).await?;
    durable_account_heads_from(store, &rows).await
}

async fn durable_account_heads_from(
    store: &SqliteStoreHandle,
    rows: &DurableAccountReceiptRows,
) -> Result<HashMap<String, DurableAccountHead>, HaiderError> {
    let mut heads = HashMap::new();
    for row in &rows.login {
        if row.state != "committed" {
            continue;
        }
        let identity: LoginIdentity = serde_json::from_str(&row.request_json).map_err(|error| {
            HaiderError::new(
                ErrorCode::StoreCorrupt,
                format!(
                    "login receipt {} has undecodable request coordinates: {error}",
                    row.command_id
                ),
                false,
            )
        })?;
        let revision = match row.final_revision {
            Some(revision) => revision,
            None => {
                store
                    .ensure_committed_management_revision(
                        row.command_id.clone(),
                        "account.login_api".to_owned(),
                    )
                    .await?
            }
        };
        record_durable_account_head(
            &mut heads,
            identity.physical_alias,
            DurableAccountHead {
                command_id: row.command_id.clone(),
                revision,
                mutation: DurableAccountMutation::Add,
            },
        )?;
    }
    for row in &rows.oauth_add {
        if row.state != "committed" {
            continue;
        }
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
        let revision = match row.final_revision {
            Some(revision) => revision,
            None => {
                store
                    .ensure_committed_management_revision(
                        row.command_id.clone(),
                        "account.add".to_owned(),
                    )
                    .await?
            }
        };
        record_durable_account_head(
            &mut heads,
            identity.alias().to_owned(),
            DurableAccountHead {
                command_id: row.command_id.clone(),
                revision,
                mutation: DurableAccountMutation::Add,
            },
        )?;
    }
    for row in &rows.remove {
        if row.state != "committed" {
            continue;
        }
        let identity: RemoveIdentity =
            serde_json::from_str(&row.request_json).map_err(|error| {
                HaiderError::new(
                    ErrorCode::StoreCorrupt,
                    format!(
                        "account.remove receipt {} has undecodable request coordinates: {error}",
                        row.command_id
                    ),
                    false,
                )
            })?;
        let receipt: RemoveReceipt = row
            .response_json
            .as_deref()
            .and_then(|json| serde_json::from_str(json).ok())
            .ok_or_else(|| {
                HaiderError::new(
                    ErrorCode::StoreCorrupt,
                    format!(
                        "committed account.remove receipt {} has no response",
                        row.command_id
                    ),
                    false,
                )
            })?;
        if receipt.removed_alias.as_str() != identity.alias {
            return Err(HaiderError::new(
                ErrorCode::StoreCorrupt,
                format!(
                    "committed account.remove receipt {} does not match its fenced alias",
                    row.command_id
                ),
                false,
            ));
        }
        let revision = row.final_revision.ok_or_else(|| {
            HaiderError::new(
                ErrorCode::StoreCorrupt,
                "committed account.remove receipt has no final revision",
                false,
            )
        })?;
        record_durable_account_head(
            &mut heads,
            identity.alias,
            DurableAccountHead {
                command_id: row.command_id.clone(),
                revision,
                mutation: DurableAccountMutation::Remove,
            },
        )?;
    }
    Ok(heads)
}

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
#[cfg(test)]
pub(crate) async fn reconcile_login_receipts(
    store: &SqliteStoreHandle,
    accounts: &mut AccountStore<Box<dyn StoreLike>>,
    vault: &VaultProvision,
) -> Result<(), HaiderError> {
    let rows = DurableAccountReceiptRows::load(store).await?;
    let durable_heads = durable_account_heads_from(store, &rows).await?;
    let reserved = store
        .reserved_account_aliases()
        .await?
        .into_iter()
        .collect::<HashSet<_>>();
    reconcile_login_receipts_from(
        store,
        accounts,
        vault,
        &rows.login,
        &durable_heads,
        &reserved,
    )
    .await
}

async fn reconcile_login_receipts_from(
    store: &SqliteStoreHandle,
    accounts: &mut AccountStore<Box<dyn StoreLike>>,
    vault: &VaultProvision,
    rows: &[LoginReceiptRow],
    durable_heads: &HashMap<String, DurableAccountHead>,
    reserved: &HashSet<String>,
) -> Result<(), HaiderError> {
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
                if !durable_heads.get(alias.as_str()).is_some_and(|head| {
                    head.mutation == DurableAccountMutation::Add
                        && head.command_id == row.command_id
                }) {
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
                if identity.replace_existing {
                    // An old descriptor is expected to exist throughout an
                    // in-place re-key, so neither descriptor nor vault
                    // presence proves which side of validation/replace a
                    // crash reached. Same-command retry with a fresh stage
                    // is the only unambiguous recovery.
                    continue;
                }
                if accounts.get(&alias).is_some() {
                    finalize_reconciled(store, accounts, &row.command_id, &alias).await?;
                    continue;
                }
                let VaultProvision::Available(vault) = vault else {
                    continue;
                };
                if vault.resolve(&alias).is_ok() {
                    let descriptor = descriptor_for(&identity, &alias, None, None);
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

#[cfg(test)]
pub(crate) async fn reconcile_oauth_add_receipts(
    store: &SqliteStoreHandle,
    accounts: &mut AccountStore<Box<dyn StoreLike>>,
    vault: &VaultProvision,
) -> Result<(), HaiderError> {
    let rows = DurableAccountReceiptRows::load(store).await?;
    let durable_heads = durable_account_heads_from(store, &rows).await?;
    let reserved = store
        .reserved_account_aliases()
        .await?
        .into_iter()
        .collect::<HashSet<_>>();
    reconcile_oauth_add_receipts_from(
        store,
        accounts,
        vault,
        &rows.oauth_add,
        &durable_heads,
        &reserved,
    )
    .await
}

async fn reconcile_oauth_add_receipts_from(
    store: &SqliteStoreHandle,
    accounts: &mut AccountStore<Box<dyn StoreLike>>,
    vault: &VaultProvision,
    rows: &[AccountAddReceiptRow],
    durable_heads: &HashMap<String, DurableAccountHead>,
    reserved: &HashSet<String>,
) -> Result<(), HaiderError> {
    for row in rows {
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
                if !durable_heads.get(alias.as_str()).is_some_and(|head| {
                    head.mutation == DurableAccountMutation::Add
                        && head.command_id == row.command_id
                }) {
                    continue;
                }
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
    // The committed receipt is the durable delete paired with the add
    // receipts above. Apply only the lifecycle head: an older remove cannot
    // erase a later re-add of the same alias, and no neighboring alias is
    // selected by the revision fence.
    for (alias, head) in durable_account_heads(store).await? {
        if head.mutation == DurableAccountMutation::Remove {
            let alias = CredentialAlias::new(alias);
            if accounts.get(&alias).is_some() {
                accounts.remove(&alias)?;
            }
        }
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
    accounts: &AccountStore<Box<dyn StoreLike>>,
    providers: &mut ProviderRegistry<Box<dyn ProviderRegistryStoreLike>>,
) -> Result<(), HaiderError> {
    let rows = store.provider_management_receipts().await?;
    let mut latest_existence_mutation = HashMap::new();
    for (index, row) in rows.iter().enumerate() {
        let provider = match row.method.as_str() {
            PROVIDER_CONFIGURE_METHOD => {
                let identity: ProviderConfigureIdentity = serde_json::from_str(&row.request_json)
                    .map_err(|error| {
                    HaiderError::new(
                        ErrorCode::StoreCorrupt,
                        format!("provider-configure identity is invalid: {error}"),
                        false,
                    )
                })?;
                Some(identity.input.provider)
            }
            PROVIDER_REMOVE_METHOD => {
                let identity: ProviderRemoveIdentity = serde_json::from_str(&row.request_json)
                    .map_err(|error| {
                        HaiderError::new(
                            ErrorCode::StoreCorrupt,
                            format!("provider-remove identity is invalid: {error}"),
                            false,
                        )
                    })?;
                Some(identity.provider)
            }
            ACCOUNT_SET_DEFAULT_MODEL_METHOD | PROVIDER_SET_TRUST_METHOD => None,
            _ => {
                return Err(HaiderError::new(
                    ErrorCode::StoreCorrupt,
                    format!("unexpected provider management method `{}`", row.method),
                    false,
                ));
            }
        };
        if let Some(provider) = provider {
            latest_existence_mutation.insert(provider, index);
        }
    }

    for (index, row) in rows.into_iter().enumerate() {
        if row.state == "committed" {
            if row.final_revision.is_none() {
                store
                    .ensure_committed_management_revision(
                        row.command_id.clone(),
                        row.method.clone(),
                    )
                    .await?;
            }
            if row.method == PROVIDER_REMOVE_METHOD {
                let identity: ProviderRemoveIdentity = serde_json::from_str(&row.request_json)
                    .map_err(|error| {
                        HaiderError::new(
                            ErrorCode::StoreCorrupt,
                            format!("provider-remove identity is invalid: {error}"),
                            false,
                        )
                    })?;
                if latest_existence_mutation.get(&identity.provider) == Some(&index) {
                    providers.reconcile_remove(&identity.provider)?;
                }
            }
            continue;
        }
        let (profile, revision_unchanged, recovered_catalog, auth_change) = match row
            .method
            .as_str()
        {
            ACCOUNT_SET_DEFAULT_MODEL_METHOD => {
                let identity: SetDefaultModelIdentity = serde_json::from_str(&row.request_json)
                    .map_err(|error| {
                        HaiderError::new(
                            ErrorCode::StoreCorrupt,
                            format!("pending default-model identity is invalid: {error}"),
                            false,
                        )
                    })?;
                (
                    providers.reconcile_set_default_model(&identity.provider, &identity.model)?,
                    false,
                    None,
                    None,
                )
            }
            PROVIDER_SET_TRUST_METHOD => {
                let identity: ProviderSetTrustIdentity = serde_json::from_str(&row.request_json)
                    .map_err(|error| {
                        HaiderError::new(
                            ErrorCode::StoreCorrupt,
                            format!("pending provider-trust identity is invalid: {error}"),
                            false,
                        )
                    })?;
                let recovery: ProviderSetTrustRecovery = row
                    .recovery_json
                    .as_deref()
                    .and_then(|json| serde_json::from_str(json).ok())
                    .ok_or_else(|| {
                        HaiderError::new(
                            ErrorCode::StoreCorrupt,
                            "pending provider-trust receipt has no recovery coordinates",
                            false,
                        )
                    })?;
                let anticipated_revision =
                    identity.expected_revision.checked_add(1).ok_or_else(|| {
                        HaiderError::new(
                            ErrorCode::StoreCorrupt,
                            "provider trust revision space is exhausted during reconciliation",
                            false,
                        )
                    })?;
                let profile = providers.set_trust(&identity.provider, identity.trust)?;
                journal_provider_trust_changed(
                    store,
                    &row.command_id,
                    &identity.provider,
                    recovery.previous,
                    identity.trust,
                    anticipated_revision,
                )
                .await?;
                (profile, false, None, None)
            }
            PROVIDER_CONFIGURE_METHOD => {
                let identity: ProviderConfigureIdentity = serde_json::from_str(&row.request_json)
                    .map_err(|error| {
                    HaiderError::new(
                        ErrorCode::StoreCorrupt,
                        format!("pending provider-configure identity is invalid: {error}"),
                        false,
                    )
                })?;
                let recovery: ProviderConfigureRecovery = row
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
                let revision_unchanged = recovery.revision_unchanged;
                let auth_change = recovery
                    .previous_auth_requirement
                    .zip(recovery.input.auth_requirement)
                    .filter(|(previous, current)| previous != current)
                    .map(|(previous, current)| {
                        identity
                            .expected_revision
                            .checked_add(1)
                            .map(|revision| {
                                (recovery.input.provider.clone(), previous, current, revision)
                            })
                            .ok_or_else(|| {
                                HaiderError::new(
                                    ErrorCode::StoreCorrupt,
                                    "provider auth revision space is exhausted during reconciliation",
                                    false,
                                )
                            })
                    })
                    .transpose()?;
                let recovered_catalog =
                    recovery.discovered_models.map(|models| DiscoveredCatalog {
                        models,
                        etag: recovery.discovered_etag,
                    });
                let profile = if revision_unchanged {
                    providers
                        .get(&recovery.input.provider)
                        .cloned()
                        .ok_or_else(|| {
                            HaiderError::new(
                                ErrorCode::StoreCorrupt,
                                "pending no-op provider configuration targets a missing provider",
                                false,
                            )
                        })?
                } else {
                    providers.reconcile_configure(recovery.input)?
                };
                (profile, revision_unchanged, recovered_catalog, auth_change)
            }
            PROVIDER_REMOVE_METHOD => {
                let identity: ProviderRemoveIdentity = serde_json::from_str(&row.request_json)
                    .map_err(|error| {
                        HaiderError::new(
                            ErrorCode::StoreCorrupt,
                            format!("provider-remove identity is invalid: {error}"),
                            false,
                        )
                    })?;
                if latest_existence_mutation.get(&identity.provider) != Some(&index) {
                    return Err(HaiderError::new(
                        ErrorCode::StoreCorrupt,
                        format!(
                            "pending provider-remove `{}` was superseded by a later mutation",
                            row.command_id
                        ),
                        false,
                    ));
                }
                providers.reconcile_remove(&identity.provider)?;
                store
                    .finalize_provider_remove_receipt(
                        row.command_id,
                        identity.provider.clone(),
                        ProviderRemoveReceipt {
                            provider: identity.provider,
                        },
                    )
                    .await?;
                continue;
            }
            _ => {
                return Err(HaiderError::new(
                    ErrorCode::StoreCorrupt,
                    format!(
                        "unexpected pending provider management method `{}`",
                        row.method
                    ),
                    false,
                ));
            }
        };
        if let Some((provider, previous, auth_requirement, revision)) = auth_change {
            journal_provider_auth_changed(
                store,
                &row.command_id,
                &provider,
                previous,
                auth_requirement,
                revision,
            )
            .await?;
        }
        if let Some(catalog) = recovered_catalog {
            let models_json = serde_json::to_string(&catalog.models).map_err(|error| {
                HaiderError::new(
                    ErrorCode::StoreCorrupt,
                    format!("recovered provider model catalog is invalid: {error}"),
                    false,
                )
            })?;
            let fetched_at_ms = unix_ms_after(Duration::ZERO);
            store
                .put_provider_models(
                    profile.provider_id.clone(),
                    models_json,
                    catalog.etag,
                    fetched_at_ms,
                )
                .await?;
            providers.replace_models(
                profile.provider_id.clone(),
                catalog.models,
                Some(fetched_at_ms),
            );
        }
        let summary = providers
            .summary(&profile.provider_id, &provider_has_credential(accounts))
            .ok_or_else(|| {
                HaiderError::new(
                    ErrorCode::StoreCorrupt,
                    "reconciled provider disappeared before receipt finalization",
                    false,
                )
            })?;
        store
            .finalize_management_receipt(
                row.command_id,
                row.method,
                ProviderReceipt {
                    provider: summary,
                    revision_unchanged,
                },
            )
            .await?;
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
    /// Typed projection of the durable provider catalog, shared with the
    /// turn factory so pinned adapters never need to refetch capability
    /// metadata.
    pub model_source: Arc<CachedProviderModelSource>,
    /// The vault provision the production provider factory shares.
    pub vault: VaultProvision,
    pub broker: Option<CredentialBroker>,
    /// Immutable, startup-validated resilience selection captured from the
    /// registry plus one-shot environment overrides.
    pub resilience: AccountsResilienceConfig,
}

/// The environment variable AWS documents for Bedrock bearer keys; the
/// startup env bridge (G4b, LA-x) imports it once when set.
pub(crate) const BEDROCK_ENV_BEARER_VAR: &str = "AWS_BEARER_TOKEN_BEDROCK";

/// One-shot startup import of `AWS_BEARER_TOKEN_BEDROCK` through the
/// accounts env bridge (G4b, LA-x): when the variable is set AND no bedrock
/// descriptor exists yet, the value is vaulted under the deterministic
/// `bedrock-env` alias and a descriptor lands active. An explicit login or
/// removal is never fought — any existing bedrock descriptor (any status)
/// suppresses the import entirely. Best-effort by design: a malformed value
/// or vault failure skips the import rather than failing daemon boot, and
/// resolution never consults the environment again (the bridge contract).
fn import_bedrock_env_bearer(
    accounts: &mut AccountStore<Box<dyn StoreLike>>,
    vault: &VaultProvision,
) {
    if std::env::var_os(BEDROCK_ENV_BEARER_VAR).is_none()
        || accounts
            .list()
            .iter()
            .any(|descriptor| descriptor.provider == BEDROCK_PROVIDER_NAME)
    {
        return;
    }
    let VaultProvision::Available(vault) = vault else {
        return;
    };
    let Ok(alias) = haider_accounts::import_env(
        vault.as_ref(),
        BEDROCK_PROVIDER_NAME,
        BEDROCK_ENV_BEARER_VAR,
    ) else {
        return;
    };
    let account_identity = vault
        .resolve(&alias)
        .ok()
        .map(|secret| api_key_identity(BEDROCK_PROVIDER_NAME, secret.expose_secret()));
    let descriptor = CredentialDescriptor {
        alias: alias.clone(),
        provider: BEDROCK_PROVIDER_NAME.to_owned(),
        base_url: None,
        auth_method: AuthMethod::ApiKey,
        identity: format!("{BEDROCK_ENV_BEARER_VAR} (env import)"),
        status: CredentialStatus::Ok,
        active: true,
        label: None,
        account_identity,
        created_at_ms: None,
    };
    if accounts.add(descriptor).is_err() {
        // The descriptor store refused (never a duplicate — checked above);
        // drop the orphaned vault entry so the next boot retries cleanly.
        let _ = vault.delete(&alias);
    }
}

impl AccountsRuntime {
    /// Loads the descriptor store, runs receipt reconciliation, and starts
    /// the account actor (vault-supported platforms only).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn initialize(
        store: &SqliteStoreHandle,
        dependencies: &AccountsDependencies,
        store_dir: &std::path::Path,
        profile_id: &str,
        instance_id: &str,
        default_model: &str,
        provider_names: &std::collections::BTreeSet<String>,
        discovery_disabled: bool,
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
            .summaries(&|_| false)
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
                model_source.replace(provider.clone(), models, Some(cached.fetched_at_ms));
            }
        }
        // R10: every daemon vault consumer funnels through this ONE wrap.
        // Global aliases are the descriptor coordinate, but the Keychain is
        // machine-global — the physical key must stay profile-scoped so two
        // profiles' identical aliases can never collide (see profile_vault.rs).
        // Resolve the SOURCE secret store first: an injected vault (tests,
        // an explicit override) is used as-is; the platform default is the
        // profile's own file vault under the store dir (W5f-4); `Unsupported`
        // stays an explicit opt-out that rejects login. `PlatformDefault` is
        // consumed HERE and never reaches the matches below.
        let source: Option<Arc<dyn Vault>> = match &dependencies.vault {
            VaultProvision::Available(inner) => Some(Arc::clone(inner)),
            VaultProvision::PlatformDefault => Some(Arc::new(haider_accounts::FileVault::new(
                store_dir.join("vault"),
            )) as Arc<dyn Vault>),
            VaultProvision::Unsupported => None,
        };
        // R10: every daemon vault consumer funnels through this ONE wrap.
        // Global aliases are the descriptor coordinate, but the physical key
        // must stay profile-scoped so two profiles' identical aliases can
        // never collide (see profile_vault.rs).
        let vault = match source {
            Some(inner) => VaultProvision::Available(Arc::new(
                crate::profile_vault::ProfileVault::new(inner, profile_id),
            ) as Arc<dyn Vault>),
            None => VaultProvision::Unsupported,
        };
        reconcile_remove_receipts(store, &mut accounts, &vault).await?;
        // Removal reconciliation can finalize a reservation, so hydrate only
        // after it completes. Login and OAuth then share one coherent receipt
        // snapshot and the actor reuses the same post-removal alias fence.
        // This is a single-writer pre-Ready window: no concurrent account
        // command can invalidate the snapshot between these consumers.
        let receipt_rows = DurableAccountReceiptRows::load(store).await?;
        let durable_heads = durable_account_heads_from(store, &receipt_rows).await?;
        let reserved_aliases = store
            .reserved_account_aliases()
            .await?
            .into_iter()
            .collect::<HashSet<_>>();
        reconcile_login_receipts_from(
            store,
            &mut accounts,
            &vault,
            &receipt_rows.login,
            &durable_heads,
            &reserved_aliases,
        )
        .await?;
        reconcile_oauth_add_receipts_from(
            store,
            &mut accounts,
            &vault,
            &receipt_rows.oauth_add,
            &durable_heads,
            &reserved_aliases,
        )
        .await?;
        reconcile_set_active_receipts(store, &mut accounts).await?;
        reconcile_provider_receipts(store, &accounts, &mut providers).await?;
        import_bedrock_env_bearer(&mut accounts, &vault);
        let snapshot: AccountsSnapshot = Arc::new(StdMutex::new(accounts.list().to_vec()));
        let management = ManagementSnapshot::new(
            store.management_revision().await?,
            accounts.list().to_vec(),
            providers.summaries(&provider_has_credential(&accounts)),
        );
        let device_discovery = DeviceDiscoverySnapshot::new(discovery_disabled);
        let promotion_targets = management
            .read()
            .map(|view| {
                view.providers
                    .into_iter()
                    .filter_map(|summary| {
                        providers
                            .compaction_promotion(&summary.provider)
                            .filter(|target| target.provider == summary.provider)
                            .map(|target| (summary.provider, target))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let resilience = AccountsResilienceConfig {
            fallback_chain: providers.fallback_chain().to_vec(),
            promotion_targets,
        };
        let actor_vault: Arc<dyn Vault> = match &vault {
            VaultProvision::Available(vault) => Arc::clone(vault),
            VaultProvision::Unsupported => Arc::new(MemoryVault::default()),
            VaultProvision::PlatformDefault => {
                unreachable!("PlatformDefault is resolved to a concrete vault above")
            }
        };
        let refresh_fences = RefreshFenceRegistry::default();
        let actor_config = AccountActorConfig {
            store: store.clone(),
            accounts,
            vault: actor_vault,
            validator: Arc::clone(&dependencies.validator),
            snapshot: Arc::clone(&snapshot),
            management: Some(management.clone()),
            device_discovery: device_discovery.clone(),
            profile_id: profile_id.to_owned(),
            default_model: default_model.to_owned(),
            providers,
            provider_endpoint_validator: Arc::clone(&dependencies.provider_endpoint_validator),
            reserved_aliases,
            refresh_fences: refresh_fences.clone(),
        };
        match &vault {
            VaultProvision::Available(scoped) => {
                let oauth = OAuthCoordinator::new_with_vault(
                    instance_id.to_owned(),
                    dependencies.oauth_catalog.clone(),
                    dependencies.oauth_coordinator,
                    Arc::clone(scoped),
                )
                .map_err(crate::oauth::oauth_error)?;
                let gcloud = Arc::clone(&dependencies.gcloud);
                let (actor, broker) = start_account_actor_with_services(
                    actor_config,
                    |commands| {
                        CredentialBroker::new_with_fences(
                            Arc::clone(scoped),
                            dependencies.oauth_catalog.clone(),
                            Arc::clone(&snapshot),
                            commands,
                            refresh_fences,
                        )?
                        .with_gcloud_source(Arc::clone(&gcloud))
                    },
                    Arc::new(ProductionProviderModelDiscoverer),
                    Arc::clone(&gcloud),
                    Arc::new(PlatformClaudeNativeCredentialStore::default()),
                )?;
                let commands = actor.commands();
                Ok(Self {
                    facade: AccountsFacade {
                        login: Some(commands),
                        oauth: Some(oauth),
                        snapshot,
                        management,
                        vault_supported: true,
                        discovery_disabled,
                        device_discovery,
                        vault: Some(Arc::clone(scoped)),
                    },
                    actor: Some(actor),
                    model_source: Arc::clone(&model_source),
                    vault,
                    broker: Some(broker),
                    resilience,
                })
            }
            VaultProvision::PlatformDefault => {
                unreachable!("PlatformDefault is resolved to a concrete vault above")
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
                        discovery_disabled,
                        device_discovery,
                        vault: None,
                    },
                    actor: Some(actor),
                    model_source: Arc::clone(&model_source),
                    vault: VaultProvision::Unsupported,
                    broker: None,
                    resilience,
                })
            }
        }
    }
}

#[cfg(test)]
#[path = "accounts_tests.rs"]
mod accounts_tests;
