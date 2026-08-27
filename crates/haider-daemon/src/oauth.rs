//! Daemon-owned OAuth authorization-code/PKCE and credential refresh.
//!
//! Live provider grants are a release-owned allowlist, not user
//! configuration. Only the literal registrations in
//! [`SANCTIONED_PROVIDER_REGISTRATIONS`] are enabled; tests inject a
//! loopback-only fake registration through [`AccountsDependencies`].

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::io::Read as _;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use haider_accounts::{OAuthIdentityV1, OAuthTokenBundleV1};
use haider_accounts::{SecretHandle, Vault, VaultRefreshLock};
use haider_protocol::credential::{
    AccountIdentity, AuthMethod, CredentialDescriptor, CredentialStatus,
};
use haider_protocol::error::{ErrorAction, ErrorCode, ErrorPresentation, ErrorScope, HaiderError};
use haider_protocol::ids::CredentialAlias;
use haider_provider::Provider as _;
use haider_provider::{FixedDnsResolver, FixedOriginGuard, SystemFixedDnsResolver};
use haider_rpc::{
    ERROR_CODE_BUSY, OAuthAuthorizationWire, OAuthAvailabilityWire, OAuthFlowId,
    OAuthFlowStatusWire, OAuthImportSourceUnavailableCodeWire,
    OAuthImportSourceUnavailableReasonWire, OAuthImportSourceWire, OAuthReadyRefWire, RequestId,
    ResponseBody, WireFrame,
};
use reqwest::redirect::Policy;
use serde::de::Visitor;
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio::task::JoinSet;
use tokio::time::Instant;
use url::Url;
use zeroize::{Zeroize, Zeroizing};

use crate::session_hub::FrameSink;

const CALLBACK_RESPONSE_LIMIT: usize = 8 * 1024;
const TOKEN_RESPONSE_LIMIT: usize = 256 * 1024;
/// Sweep budget for [`scrub_source_chunks`]: hyper's connection task releases
/// its transient read-buffer reference within a few scheduler ticks of the
/// response drop, so this bound is generous; it exists only so a chunk that
/// never becomes exclusive is a bounded residual instead of a hang.
const SCRUB_YIELD_BOUND: usize = 256;
const IMPORT_FILE_LIMIT: u64 = 256 * 1024;
const MIN_RANDOM_BYTES: usize = 32;
const MAX_TOKEN_LIFETIME_SECS: u64 = 366 * 24 * 60 * 60;
const CALLBACK_READ_TIMEOUT: Duration = Duration::from_secs(2);
const TOKEN_TIMEOUT: Duration = Duration::from_secs(15);
const OAUTH_REFRESH_SKEW: Duration = Duration::from_secs(30);
const KIMI_REFRESH_REJECTED_TTL: Duration = Duration::from_secs(300);
pub(crate) const KIMI_DEVICE_ALIAS: &str = "_kimi_oauth_device_id_v1";
const KIMI_DEVICE_POLL_SLOW_DOWN: Duration = Duration::from_secs(5);
const KIMI_REFRESH_BACKOFF_INITIAL: Duration = Duration::from_millis(250);
const KIMI_REFRESH_MAX_ATTEMPTS: usize = 3;
// Codex auth.json has no explicit expiry outside the access-token JWT. An
// unparseable import is stamped this far ahead and marked inside its vault
// bundle for one eager refresh on first resolution.
const CODEX_IMPORT_FALLBACK_WINDOW: Duration = Duration::from_secs(15 * 60);
#[cfg(all(not(test), any(target_os = "macos", target_os = "windows")))]
pub(crate) const CLAUDE_CODE_CREDENTIAL_SERVICE: &str = "Claude Code-credentials";
pub(crate) const CLAUDE_DEFAULT_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub(crate) const CLAUDE_NATIVE_IDENTITY_LABEL: &str = "Linked to Claude Code";

pub(crate) fn is_claude_native_owner_identity(identity: &str) -> bool {
    identity
        .rsplit(" · ")
        .next()
        .is_some_and(|label| label.eq_ignore_ascii_case(CLAUDE_NATIVE_IDENTITY_LABEL))
}

#[cfg(test)]
static OAUTH_IMPORT_READ_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
pub(crate) fn reset_oauth_import_read_count() {
    OAUTH_IMPORT_READ_COUNT.store(0, Ordering::SeqCst);
}

#[cfg(test)]
pub(crate) fn oauth_import_read_count() -> usize {
    OAUTH_IMPORT_READ_COUNT.load(Ordering::SeqCst)
}

struct OwnedTaskSet {
    sealed: AtomicBool,
    panicked: AtomicBool,
    tasks: tokio::sync::Mutex<JoinSet<()>>,
}

impl OwnedTaskSet {
    fn new() -> Self {
        Self {
            sealed: AtomicBool::new(false),
            panicked: AtomicBool::new(false),
            tasks: tokio::sync::Mutex::new(JoinSet::new()),
        }
    }

    fn inspect_join(&self, completed: Result<(), tokio::task::JoinError>) {
        if completed.is_err_and(|error| error.is_panic()) {
            self.panicked.store(true, Ordering::Release);
            // A task owner that lost a child invariant may drain, but it may
            // not admit more children or call its shutdown graceful.
            self.seal();
        }
    }

    fn seal(&self) {
        self.sealed.store(true, Ordering::Release);
    }

    fn spawn_initial(&self, task: impl Future<Output = ()> + Send + 'static) -> bool {
        let Ok(mut tasks) = self.tasks.try_lock() else {
            return false;
        };
        if self.sealed.load(Ordering::Acquire) {
            return false;
        }
        tasks.spawn(task);
        true
    }

    async fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) -> bool {
        loop {
            if self.sealed.load(Ordering::Acquire) {
                return false;
            }
            if let Ok(mut tasks) = self.tasks.try_lock() {
                if self.sealed.load(Ordering::Acquire) {
                    return false;
                }
                while let Some(completed) = tasks.try_join_next() {
                    self.inspect_join(completed);
                }
                if self.sealed.load(Ordering::Acquire) {
                    return false;
                }
                tasks.spawn(task);
                return true;
            }
            // Never wait on the registration lock: shutdown joins tasks while
            // holding it. Rechecking the seal lets an in-progress start
            // worker leave instead of deadlocking the owner that is joining
            // that same worker.
            tokio::task::yield_now().await;
        }
    }

    async fn join_all(&self) -> bool {
        self.seal();
        let mut tasks = self.tasks.lock().await;
        while let Some(completed) = tasks.join_next().await {
            self.inspect_join(completed);
        }
        !self.panicked.load(Ordering::Acquire)
    }

    async fn abort_and_join(&self) {
        self.seal();
        let mut tasks = self.tasks.lock().await;
        tasks.abort_all();
        while let Some(completed) = tasks.join_next().await {
            self.inspect_join(completed);
        }
    }
}

/// Release-owned provider metadata shape.
///
/// OWNER FILL POINT: this table is populated only after Haider receives a
/// sanctioned public-native client registration and documented inference
/// scopes. There is no wildcard or user-supplied live-provider path.
#[derive(Debug, Clone, Copy)]
pub struct SanctionedOAuthRegistration {
    pub provider_id: &'static str,
    pub issuer: &'static str,
    pub authorization_endpoint: &'static str,
    pub token_endpoint: &'static str,
    pub client_id: &'static str,
    pub scopes: &'static [&'static str],
    pub audience: &'static str,
    pub resource: Option<&'static str>,
    pub redirect_policy: OAuthRedirectPolicy,
    pub authorize_parameters: &'static [OAuthAuthorizeParameter],
    pub send_nonce_in_authorize: bool,
    pub send_audience_in_authorize: bool,
    pub authorization_code_encoding: OAuthTokenRequestEncoding,
    pub authorization_code_includes_state: bool,
    pub refresh_encoding: OAuthTokenRequestEncoding,
    pub refresh_includes_binding: bool,
    pub retain_refresh_on_omission: bool,
    pub identity_mode: OAuthIdentityMode,
    pub inference: OAuthInferenceRegistration,
    pub flow_mode: OAuthFlowMode,
    pub auth_header_set: OAuthAuthHeaderSet,
    pub refresh_policy: OAuthRefreshPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthFlowMode {
    AuthorizationCode,
    DeviceCode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthAuthHeaderSet {
    Standard,
    KimiMsh,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthRefreshPolicy {
    Conservative,
    SerializedRotating,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OAuthAuthorizeParameter {
    pub name: &'static str,
    pub value: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthTokenRequestEncoding {
    Form,
    Json,
}

#[derive(Debug, Clone, Copy)]
pub enum OAuthIdentityMode {
    VerifiedIdToken(fn() -> Arc<dyn OAuthIdentityVerifier>),
    /// Least-privilege providers that return no ID token or profile scope.
    ///
    /// The TLS-authenticated token endpoint proves the grant. Haider stores
    /// only a one-way access-token fingerprint as the stable local subject;
    /// refresh keeps the original identity.
    TokenEndpointGrant {
        display_identity: &'static str,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OAuthInferenceRegistration {
    pub base_url: &'static str,
    pub auth_mode: OAuthInferenceAuthMode,
    pub header_set: OAuthInferenceHeaderSet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthInferenceAuthMode {
    Bearer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthInferenceHeaderSet {
    OpenAiCodexResponsesLite,
    AnthropicOAuthBeta,
    KimiOpenAiChatCompletions,
    GrokOpenAiChatCompletions,
}

/// The only callback policy supported by the generic engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthRedirectPolicy {
    /// Bind a fresh numeric `127.0.0.1:0` listener and use a random path.
    EphemeralIpv4Loopback,
}

const OPENAI_AUTHORIZE_PARAMETERS: &[OAuthAuthorizeParameter] = &[
    OAuthAuthorizeParameter {
        name: "id_token_add_organizations",
        value: "true",
    },
    OAuthAuthorizeParameter {
        name: "codex_cli_simplified_flow",
        value: "true",
    },
];

/// The complete release-owned OAuth allowlist. No other provider identifier,
/// wildcard, endpoint, client ID, or scope can enter the production catalog.
pub const SANCTIONED_PROVIDER_REGISTRATIONS: &[SanctionedOAuthRegistration] = &[
    SanctionedOAuthRegistration {
        provider_id: haider_provider::OPENAI_OAUTH_PROVIDER_NAME,
        issuer: "https://auth.openai.com",
        authorization_endpoint: "https://auth.openai.com/oauth/authorize",
        token_endpoint: "https://auth.openai.com/oauth/token",
        client_id: "app_EMoamEEZ73f0CkXaXp7hrann",
        scopes: &["openid", "profile", "email", "offline_access"],
        audience: "app_EMoamEEZ73f0CkXaXp7hrann",
        resource: None,
        redirect_policy: OAuthRedirectPolicy::EphemeralIpv4Loopback,
        authorize_parameters: OPENAI_AUTHORIZE_PARAMETERS,
        send_nonce_in_authorize: true,
        send_audience_in_authorize: false,
        authorization_code_encoding: OAuthTokenRequestEncoding::Form,
        authorization_code_includes_state: false,
        refresh_encoding: OAuthTokenRequestEncoding::Json,
        refresh_includes_binding: false,
        retain_refresh_on_omission: true,
        identity_mode: OAuthIdentityMode::VerifiedIdToken(openai_identity_verifier),
        inference: OAuthInferenceRegistration {
            base_url: haider_provider::OPENAI_SUBSCRIPTION_BASE_URL,
            auth_mode: OAuthInferenceAuthMode::Bearer,
            header_set: OAuthInferenceHeaderSet::OpenAiCodexResponsesLite,
        },
        flow_mode: OAuthFlowMode::AuthorizationCode,
        auth_header_set: OAuthAuthHeaderSet::Standard,
        refresh_policy: OAuthRefreshPolicy::Conservative,
    },
    SanctionedOAuthRegistration {
        provider_id: haider_provider::ANTHROPIC_OAUTH_PROVIDER_NAME,
        issuer: "https://claude.ai",
        authorization_endpoint: "https://claude.ai/oauth/authorize",
        token_endpoint: "https://console.anthropic.com/v1/oauth/token",
        client_id: "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
        // Claude Code 2.1.220's exact scope set, same order (W5g-7): the
        // consent page derives its permission items from these, and the
        // owner's parity ask is "identical to Claude Code". `user:inference`
        // remains the one scope the turn path consumes; the rest are
        // consented capability for later waves (sessions, connectors,
        // uploads). Pre-W5g-7 single-scope grants fail the all-scopes
        // token guard and need one re-login.
        scopes: &[
            "org:create_api_key",
            "user:profile",
            "user:inference",
            "user:sessions:claude_code",
            "user:mcp_servers",
            "user:file_upload",
        ],
        audience: "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
        resource: None,
        // The generic engine keeps its safest ephemeral numeric-loopback
        // shape per registration. The fake server exercises this exact URI;
        // acceptance by Anthropic's live client remains a runtime smoke concern.
        redirect_policy: OAuthRedirectPolicy::EphemeralIpv4Loopback,
        authorize_parameters: &[],
        send_nonce_in_authorize: false,
        send_audience_in_authorize: false,
        authorization_code_encoding: OAuthTokenRequestEncoding::Json,
        authorization_code_includes_state: true,
        refresh_encoding: OAuthTokenRequestEncoding::Json,
        refresh_includes_binding: false,
        retain_refresh_on_omission: true,
        identity_mode: OAuthIdentityMode::TokenEndpointGrant {
            display_identity: "Claude Max subscription",
        },
        inference: OAuthInferenceRegistration {
            base_url: haider_provider::ANTHROPIC_OAUTH_BASE_URL,
            auth_mode: OAuthInferenceAuthMode::Bearer,
            header_set: OAuthInferenceHeaderSet::AnthropicOAuthBeta,
        },
        flow_mode: OAuthFlowMode::AuthorizationCode,
        auth_header_set: OAuthAuthHeaderSet::Standard,
        refresh_policy: OAuthRefreshPolicy::Conservative,
    },
    SanctionedOAuthRegistration {
        provider_id: haider_provider::KIMI_OAUTH_PROVIDER_NAME,
        issuer: "https://auth.kimi.com",
        authorization_endpoint: "https://auth.kimi.com/api/oauth/device_authorization",
        token_endpoint: "https://auth.kimi.com/api/oauth/token",
        client_id: "17e5f671-d194-4dfb-9706-5516cb48c098",
        scopes: &[],
        audience: "17e5f671-d194-4dfb-9706-5516cb48c098",
        resource: None,
        redirect_policy: OAuthRedirectPolicy::EphemeralIpv4Loopback,
        authorize_parameters: &[],
        send_nonce_in_authorize: false,
        send_audience_in_authorize: false,
        authorization_code_encoding: OAuthTokenRequestEncoding::Form,
        authorization_code_includes_state: false,
        refresh_encoding: OAuthTokenRequestEncoding::Form,
        refresh_includes_binding: false,
        retain_refresh_on_omission: false,
        identity_mode: OAuthIdentityMode::TokenEndpointGrant {
            display_identity: "Kimi Code subscription",
        },
        inference: OAuthInferenceRegistration {
            base_url: haider_provider::KIMI_OAUTH_BASE_URL,
            auth_mode: OAuthInferenceAuthMode::Bearer,
            header_set: OAuthInferenceHeaderSet::KimiOpenAiChatCompletions,
        },
        flow_mode: OAuthFlowMode::DeviceCode,
        auth_header_set: OAuthAuthHeaderSet::KimiMsh,
        refresh_policy: OAuthRefreshPolicy::SerializedRotating,
    },
    SanctionedOAuthRegistration {
        provider_id: haider_provider::GROK_OAUTH_PROVIDER_NAME,
        issuer: "https://auth.x.ai",
        authorization_endpoint: "https://auth.x.ai/oauth2/device/code",
        token_endpoint: "https://auth.x.ai/oauth2/token",
        client_id: "b1a00492-073a-47ea-816f-4c329264a828",
        scopes: &[
            "openid",
            "profile",
            "email",
            "offline_access",
            "grok-cli:access",
            "api:access",
            "conversations:read",
            "conversations:write",
        ],
        audience: "b1a00492-073a-47ea-816f-4c329264a828",
        resource: None,
        redirect_policy: OAuthRedirectPolicy::EphemeralIpv4Loopback,
        authorize_parameters: &[],
        send_nonce_in_authorize: false,
        send_audience_in_authorize: false,
        authorization_code_encoding: OAuthTokenRequestEncoding::Form,
        authorization_code_includes_state: false,
        refresh_encoding: OAuthTokenRequestEncoding::Form,
        refresh_includes_binding: false,
        retain_refresh_on_omission: true,
        identity_mode: OAuthIdentityMode::TokenEndpointGrant {
            display_identity: "SuperGrok/X Premium subscription",
        },
        inference: OAuthInferenceRegistration {
            base_url: haider_provider::GROK_OAUTH_BASE_URL,
            auth_mode: OAuthInferenceAuthMode::Bearer,
            header_set: OAuthInferenceHeaderSet::GrokOpenAiChatCompletions,
        },
        flow_mode: OAuthFlowMode::DeviceCode,
        auth_header_set: OAuthAuthHeaderSet::Standard,
        refresh_policy: OAuthRefreshPolicy::Conservative,
    },
];

pub(crate) fn sanctioned_inference(provider: &str) -> Option<&'static OAuthInferenceRegistration> {
    SANCTIONED_PROVIDER_REGISTRATIONS
        .iter()
        .find(|registration| registration.provider_id == provider)
        .map(|registration| &registration.inference)
}

/// Expected facts an identity verifier must authenticate.
pub struct OAuthIdentityExpectation<'a> {
    pub issuer: &'a str,
    pub audience: &'a str,
    pub nonce: &'a [u8],
}

/// Provider-specific verified-identity seam.
///
/// Implementations must verify signature, issuer, audience, and nonce. Merely
/// decoding a JWT payload is not an implementation of this trait.
#[async_trait::async_trait]
pub trait OAuthIdentityVerifier: Send + Sync {
    async fn verify(
        &self,
        id_token: &[u8],
        expected: OAuthIdentityExpectation<'_>,
    ) -> Result<OAuthIdentityV1, OAuthPublicError>;
}

const OPENAI_JWKS_ENDPOINT: &str = "https://auth.openai.com/.well-known/jwks.json";
const OPENAI_JWKS_HOST: &str = "auth.openai.com";

struct OpenAiIdentityVerifier {
    jwks_endpoint: String,
    resolver: Arc<dyn FixedDnsResolver>,
    /// The guard the LAST `verify` built — surfaced so the W5b.2a P3 pin
    /// can prove the CONNECTION resolved through it (a
    /// `.dns_resolver(…)`-only removal leaves its count at 0 while the
    /// preflight stays green).
    #[cfg(test)]
    guard_probe: std::sync::Mutex<Option<Arc<FixedOriginGuard>>>,
}

impl OpenAiIdentityVerifier {
    fn production() -> Self {
        Self {
            jwks_endpoint: OPENAI_JWKS_ENDPOINT.to_owned(),
            resolver: Arc::new(SystemFixedDnsResolver),
            #[cfg(test)]
            guard_probe: std::sync::Mutex::new(None),
        }
    }

    #[cfg(test)]
    fn new_for_test(jwks_endpoint: impl Into<String>, resolver: Arc<dyn FixedDnsResolver>) -> Self {
        Self {
            jwks_endpoint: jwks_endpoint.into(),
            resolver,
            guard_probe: std::sync::Mutex::new(None),
        }
    }

    #[cfg(test)]
    fn last_guard(&self) -> Option<Arc<FixedOriginGuard>> {
        self.guard_probe.lock().ok().and_then(|slot| slot.clone())
    }
}

fn openai_identity_verifier() -> Arc<dyn OAuthIdentityVerifier> {
    Arc::new(OpenAiIdentityVerifier::production())
}

#[derive(Deserialize)]
struct JwksDocument {
    keys: Vec<RsaJwk>,
}

#[derive(Deserialize)]
struct RsaJwk {
    kty: String,
    kid: String,
    #[serde(default)]
    alg: Option<String>,
    n: String,
    e: String,
}

#[derive(Deserialize)]
struct OpenAiIdClaims {
    sub: String,
    nonce: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[async_trait::async_trait]
impl OAuthIdentityVerifier for OpenAiIdentityVerifier {
    async fn verify(
        &self,
        id_token: &[u8],
        expected: OAuthIdentityExpectation<'_>,
    ) -> Result<OAuthIdentityV1, OAuthPublicError> {
        let token = std::str::from_utf8(id_token)
            .map_err(|_| OAuthPublicError::new("id_token_malformed", false))?;
        let header = jsonwebtoken::decode_header(token)
            .map_err(|_| OAuthPublicError::new("id_token_malformed", false))?;
        if header.alg != jsonwebtoken::Algorithm::RS256 {
            return Err(OAuthPublicError::new("id_token_algorithm_mismatch", false));
        }
        let kid = header
            .kid
            .as_deref()
            .ok_or_else(|| OAuthPublicError::new("id_token_key_missing", false))?;
        let origin_guard = Arc::new(
            FixedOriginGuard::new(
                &self.jwks_endpoint,
                OPENAI_JWKS_HOST,
                Arc::clone(&self.resolver),
            )
            .map_err(|_| OAuthPublicError::new("identity_verifier_unavailable", true))?,
        );
        #[cfg(test)]
        if let Ok(mut slot) = self.guard_probe.lock() {
            slot.replace(Arc::clone(&origin_guard));
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(TOKEN_TIMEOUT)
            .dns_resolver(Arc::clone(&origin_guard))
            .build()
            .map_err(|_| OAuthPublicError::new("identity_verifier_unavailable", true))?;
        origin_guard
            .validate_endpoint(&self.jwks_endpoint)
            .await
            .map_err(|_| OAuthPublicError::new("identity_verifier_unavailable", true))?;
        let response = client
            .get(&self.jwks_endpoint)
            .header(reqwest::header::CONNECTION, "close")
            .send()
            .await
            .map_err(|_| OAuthPublicError::new("identity_verifier_unavailable", true))?;
        if !response.status().is_success() {
            return Err(OAuthPublicError::new("identity_verifier_unavailable", true));
        }
        let jwks_bytes = bounded_jwks_response(response).await?;
        let jwks = serde_json::from_slice::<JwksDocument>(&jwks_bytes)
            .map_err(|_| OAuthPublicError::new("identity_keys_malformed", true))?;
        let jwk = jwks
            .keys
            .iter()
            .find(|key| {
                key.kid == kid
                    && key.kty == "RSA"
                    && key.alg.as_deref().is_none_or(|alg| alg == "RS256")
            })
            .ok_or_else(|| OAuthPublicError::new("id_token_key_unknown", true))?;
        let key = jsonwebtoken::DecodingKey::from_rsa_components(&jwk.n, &jwk.e)
            .map_err(|_| OAuthPublicError::new("identity_keys_malformed", true))?;
        let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
        validation.set_audience(&[expected.audience]);
        validation.set_issuer(&[expected.issuer]);
        validation.set_required_spec_claims(&["exp", "aud", "iss", "sub"]);
        let claims = jsonwebtoken::decode::<OpenAiIdClaims>(token, &key, &validation)
            .map_err(|_| OAuthPublicError::new("identity_claim_mismatch", false))?
            .claims;
        if !constant_time_equal(claims.nonce.as_bytes(), expected.nonce) {
            return Err(OAuthPublicError::new("identity_claim_mismatch", false));
        }
        let display_identity = claims
            .email
            .filter(|value| !value.trim().is_empty())
            .or_else(|| claims.name.filter(|value| !value.trim().is_empty()))
            .unwrap_or_else(|| claims.sub.clone());
        Ok(OAuthIdentityV1 {
            subject_hash: blake3::hash(claims.sub.as_bytes()).to_hex().to_string(),
            display_identity,
        })
    }
}

/// A sanitized OAuth failure. `Debug` intentionally omits endpoint bodies.
#[derive(Clone, PartialEq, Eq)]
pub struct OAuthPublicError {
    pub code: &'static str,
    pub retryable: bool,
    retryable_status: bool,
}

impl OAuthPublicError {
    pub const fn new(code: &'static str, retryable: bool) -> Self {
        Self {
            code,
            retryable,
            retryable_status: false,
        }
    }

    const fn retryable_status() -> Self {
        Self {
            code: "token_endpoint_unavailable",
            retryable: true,
            retryable_status: true,
        }
    }
}

impl fmt::Debug for OAuthPublicError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OAuthPublicError")
            .field("code", &self.code)
            .field("retryable", &self.retryable)
            .finish()
    }
}

/// Immutable registration for one approved public client.
#[derive(Clone)]
pub struct OAuthProviderRegistration {
    provider_id: String,
    issuer: String,
    authorization_endpoint: Url,
    token_endpoint: Url,
    client_id: String,
    scopes: Vec<String>,
    audience: String,
    resource: Option<String>,
    redirect_policy: OAuthRedirectPolicy,
    authorize_parameters: Vec<(String, String)>,
    send_nonce_in_authorize: bool,
    send_audience_in_authorize: bool,
    authorization_code_encoding: OAuthTokenRequestEncoding,
    authorization_code_includes_state: bool,
    refresh_encoding: OAuthTokenRequestEncoding,
    refresh_includes_binding: bool,
    retain_refresh_on_omission: bool,
    identity_mode: RuntimeIdentityMode,
    flow_mode: OAuthFlowMode,
    auth_header_set: OAuthAuthHeaderSet,
    refresh_policy: OAuthRefreshPolicy,
}

#[derive(Clone)]
enum RuntimeIdentityMode {
    VerifiedIdToken(Arc<dyn OAuthIdentityVerifier>),
    TokenEndpointGrant { display_identity: String },
}

impl fmt::Debug for OAuthProviderRegistration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OAuthProviderRegistration")
            .field("provider_id", &self.provider_id)
            .field("issuer", &self.issuer)
            .field(
                "authorization_endpoint",
                &safe_url(&self.authorization_endpoint),
            )
            .field("token_endpoint", &safe_url(&self.token_endpoint))
            .field("client_id", &self.client_id)
            .field("scopes", &self.scopes)
            .field("audience", &self.audience)
            .field("resource", &self.resource)
            .field("redirect_policy", &self.redirect_policy)
            .field(
                "retain_refresh_on_omission",
                &self.retain_refresh_on_omission,
            )
            .field("identity_mode", &"[IDENTITY VERIFIER]")
            .field("flow_mode", &self.flow_mode)
            .field("auth_header_set", &self.auth_header_set)
            .field("refresh_policy", &self.refresh_policy)
            .finish()
    }
}

impl OAuthProviderRegistration {
    /// Whether a grant must carry `scope` to pass VALIDATION (import,
    /// stored bundle, token/refresh response).
    ///
    /// `scopes` is what OUR authorize REQUESTS — consent breadth (Claude
    /// Code parity, W5g-7). A grant only needs the operation-critical
    /// subset: a foreign import or an older refresh grant that can serve
    /// inference must not be refused for missing consent-only scopes.
    /// Every scope stays required by default; only Anthropic's
    /// consent-only breadth relaxes.
    fn validation_required(&self, scope: &str) -> bool {
        if self.provider_id == haider_provider::ANTHROPIC_OAUTH_PROVIDER_NAME {
            return scope == "user:inference";
        }
        true
    }

    /// Constructs validated immutable metadata.
    ///
    /// Plain HTTP is accepted only for numeric loopback endpoints, which is
    /// the real-socket fake-server seam. Production registrations require
    /// HTTPS. There is structurally no client-secret field.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider_id: impl Into<String>,
        issuer: impl Into<String>,
        authorization_endpoint: impl AsRef<str>,
        token_endpoint: impl AsRef<str>,
        client_id: impl Into<String>,
        scopes: impl IntoIterator<Item = String>,
        audience: impl Into<String>,
        resource: Option<String>,
        retain_refresh_on_omission: bool,
        identity_verifier: Arc<dyn OAuthIdentityVerifier>,
    ) -> Result<Self, OAuthPublicError> {
        Self::new_with_identity(
            provider_id,
            issuer,
            authorization_endpoint,
            token_endpoint,
            client_id,
            scopes,
            audience,
            resource,
            retain_refresh_on_omission,
            RuntimeIdentityMode::VerifiedIdToken(identity_verifier),
        )
    }

    #[cfg(test)]
    pub(crate) fn with_test_refresh_shape(
        mut self,
        encoding: OAuthTokenRequestEncoding,
        includes_binding: bool,
    ) -> Self {
        self.refresh_encoding = encoding;
        self.refresh_includes_binding = includes_binding;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_test_device_flow(mut self) -> Self {
        self.flow_mode = OAuthFlowMode::DeviceCode;
        self.auth_header_set = OAuthAuthHeaderSet::KimiMsh;
        self.refresh_policy = OAuthRefreshPolicy::SerializedRotating;
        self.refresh_encoding = OAuthTokenRequestEncoding::Form;
        self.refresh_includes_binding = false;
        self.retain_refresh_on_omission = false;
        self.scopes.clear();
        self.identity_mode = RuntimeIdentityMode::TokenEndpointGrant {
            display_identity: "Fake device subscription".to_owned(),
        };
        self
    }

    #[allow(clippy::too_many_arguments)]
    fn new_with_identity(
        provider_id: impl Into<String>,
        issuer: impl Into<String>,
        authorization_endpoint: impl AsRef<str>,
        token_endpoint: impl AsRef<str>,
        client_id: impl Into<String>,
        scopes: impl IntoIterator<Item = String>,
        audience: impl Into<String>,
        resource: Option<String>,
        retain_refresh_on_omission: bool,
        identity_mode: RuntimeIdentityMode,
    ) -> Result<Self, OAuthPublicError> {
        let provider_id = provider_id.into();
        let issuer = issuer.into();
        let client_id = client_id.into();
        let audience = audience.into();
        let authorization_endpoint =
            Url::parse(authorization_endpoint.as_ref()).map_err(|_| invalid_metadata())?;
        let token_endpoint = Url::parse(token_endpoint.as_ref()).map_err(|_| invalid_metadata())?;
        let issuer_url = Url::parse(&issuer).map_err(|_| invalid_metadata())?;
        validate_metadata_url(&authorization_endpoint)?;
        validate_metadata_url(&token_endpoint)?;
        validate_metadata_url(&issuer_url)?;
        let scopes = scopes.into_iter().collect::<Vec<_>>();
        let unique_scopes = scopes.iter().collect::<BTreeSet<_>>();
        if provider_id.trim().is_empty()
            || issuer.trim().is_empty()
            || client_id.trim().is_empty()
            || audience.trim().is_empty()
            || scopes.iter().any(|scope| scope.trim().is_empty())
            || unique_scopes.len() != scopes.len()
        {
            return Err(invalid_metadata());
        }
        Ok(Self {
            provider_id,
            issuer,
            authorization_endpoint,
            token_endpoint,
            client_id,
            scopes,
            audience,
            resource,
            redirect_policy: OAuthRedirectPolicy::EphemeralIpv4Loopback,
            authorize_parameters: Vec::new(),
            send_nonce_in_authorize: true,
            send_audience_in_authorize: true,
            authorization_code_encoding: OAuthTokenRequestEncoding::Form,
            authorization_code_includes_state: false,
            refresh_encoding: OAuthTokenRequestEncoding::Form,
            refresh_includes_binding: true,
            retain_refresh_on_omission,
            identity_mode,
            flow_mode: OAuthFlowMode::AuthorizationCode,
            auth_header_set: OAuthAuthHeaderSet::Standard,
            refresh_policy: OAuthRefreshPolicy::Conservative,
        })
    }

    #[must_use]
    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }
}

fn validate_metadata_url(url: &Url) -> Result<(), OAuthPublicError> {
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.host_str().is_none()
    {
        return Err(invalid_metadata());
    }
    match url.scheme() {
        "https" => Ok(()),
        "http" if matches!(url.host(), Some(url::Host::Ipv4(ip)) if ip.is_loopback()) => Ok(()),
        _ => Err(invalid_metadata()),
    }
}

fn invalid_metadata() -> OAuthPublicError {
    OAuthPublicError::new("invalid_provider_metadata", false)
}

/// One availability authority shared by RPC and the later management view.
#[derive(Clone)]
pub struct OAuthProviderCatalog {
    registrations: Arc<HashMap<String, Arc<OAuthProviderRegistration>>>,
}

impl Default for OAuthProviderCatalog {
    fn default() -> Self {
        let registrations = SANCTIONED_PROVIDER_REGISTRATIONS
            .iter()
            .filter_map(|metadata| {
                if metadata.redirect_policy != OAuthRedirectPolicy::EphemeralIpv4Loopback {
                    return None;
                }
                let identity_mode = match metadata.identity_mode {
                    OAuthIdentityMode::VerifiedIdToken(factory) => {
                        RuntimeIdentityMode::VerifiedIdToken(factory())
                    }
                    OAuthIdentityMode::TokenEndpointGrant { display_identity } => {
                        RuntimeIdentityMode::TokenEndpointGrant {
                            display_identity: display_identity.to_owned(),
                        }
                    }
                };
                let mut registration = OAuthProviderRegistration::new_with_identity(
                    metadata.provider_id,
                    metadata.issuer,
                    metadata.authorization_endpoint,
                    metadata.token_endpoint,
                    metadata.client_id,
                    metadata.scopes.iter().map(|scope| (*scope).to_owned()),
                    metadata.audience,
                    metadata.resource.map(str::to_owned),
                    metadata.retain_refresh_on_omission,
                    identity_mode,
                )
                .ok()?;
                registration.authorize_parameters = metadata
                    .authorize_parameters
                    .iter()
                    .map(|parameter| (parameter.name.to_owned(), parameter.value.to_owned()))
                    .collect();
                registration.send_nonce_in_authorize = metadata.send_nonce_in_authorize;
                registration.send_audience_in_authorize = metadata.send_audience_in_authorize;
                registration.authorization_code_encoding = metadata.authorization_code_encoding;
                registration.authorization_code_includes_state =
                    metadata.authorization_code_includes_state;
                registration.refresh_encoding = metadata.refresh_encoding;
                registration.refresh_includes_binding = metadata.refresh_includes_binding;
                registration.flow_mode = metadata.flow_mode;
                registration.auth_header_set = metadata.auth_header_set;
                registration.refresh_policy = metadata.refresh_policy;
                Some((registration.provider_id.clone(), Arc::new(registration)))
            })
            .collect::<HashMap<_, _>>();
        Self {
            registrations: Arc::new(registrations),
        }
    }
}

impl OAuthProviderCatalog {
    pub fn with_test_registrations(
        registrations: impl IntoIterator<Item = OAuthProviderRegistration>,
    ) -> Result<Self, OAuthPublicError> {
        let mut by_provider = HashMap::new();
        for registration in registrations {
            // This public constructor is an integration-test seam, not an
            // alternate live-provider enablement mechanism. Release metadata
            // can only come from the immutable sanctioned table above.
            if !is_numeric_loopback_http(&registration.authorization_endpoint)
                || !is_numeric_loopback_http(&registration.token_endpoint)
            {
                return Err(invalid_metadata());
            }
            let provider = registration.provider_id.clone();
            if by_provider
                .insert(provider, Arc::new(registration))
                .is_some()
            {
                return Err(invalid_metadata());
            }
        }
        Ok(Self {
            registrations: Arc::new(by_provider),
        })
    }

    #[must_use]
    pub fn availability(&self, provider: &str, vault_supported: bool) -> OAuthAvailabilityWire {
        if !vault_supported {
            return OAuthAvailabilityWire {
                available: false,
                reason: Some(
                    "OAuth requires a supported OS credential vault; plaintext token files are not allowed"
                        .into(),
                ),
            };
        }
        if self.registrations.contains_key(provider) {
            return OAuthAvailabilityWire {
                available: true,
                reason: None,
            };
        }
        let reason = match provider {
            "openai" => {
                "Unavailable: Haider has no sanctioned ChatGPT inference client registration or scopes"
            }
            "anthropic" => {
                "Unavailable: Anthropic policy forbids third-party Claude Max subscription login"
            }
            _ => "Unavailable: this provider has no release-approved OAuth registration",
        };
        OAuthAvailabilityWire {
            available: false,
            reason: Some(reason.into()),
        }
    }

    fn registration(&self, provider: &str) -> Option<Arc<OAuthProviderRegistration>> {
        self.registrations.get(provider).cloned()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OAuthImportSourceSpec {
    pub source: &'static str,
    pub provider: &'static str,
    pub default_alias: &'static str,
    env_override: &'static str,
    home_relative_path: &'static str,
}

const OAUTH_IMPORT_SOURCE_SPECS: [OAuthImportSourceSpec; 4] = [
    OAuthImportSourceSpec {
        source: "codex",
        provider: haider_provider::OPENAI_OAUTH_PROVIDER_NAME,
        default_alias: haider_provider::OPENAI_OAUTH_PROVIDER_NAME,
        env_override: "HAIDER_CODEX_AUTH_PATH",
        home_relative_path: ".codex/auth.json",
    },
    OAuthImportSourceSpec {
        source: "claude-code",
        provider: haider_provider::ANTHROPIC_OAUTH_PROVIDER_NAME,
        default_alias: haider_provider::ANTHROPIC_OAUTH_PROVIDER_NAME,
        env_override: "HAIDER_CLAUDE_CREDS_PATH",
        home_relative_path: ".claude/.credentials.json",
    },
    OAuthImportSourceSpec {
        source: "kimi-code",
        provider: haider_provider::KIMI_OAUTH_PROVIDER_NAME,
        default_alias: haider_provider::KIMI_OAUTH_PROVIDER_NAME,
        env_override: "HAIDER_KIMI_CREDS_PATH",
        home_relative_path: ".kimi/credentials/kimi-code.json",
    },
    OAuthImportSourceSpec {
        source: "grok-cli",
        provider: haider_provider::GROK_OAUTH_PROVIDER_NAME,
        default_alias: haider_provider::GROK_OAUTH_PROVIDER_NAME,
        env_override: "HAIDER_GROK_AUTH_PATH",
        home_relative_path: ".grok/auth.json",
    },
];

pub(crate) fn oauth_import_source_spec(source: &str) -> Result<OAuthImportSourceSpec, HaiderError> {
    OAUTH_IMPORT_SOURCE_SPECS
        .iter()
        .copied()
        .find(|spec| spec.source == source)
        .ok_or_else(|| {
            HaiderError::new(
                ErrorCode::InvalidArgument,
                "OAuth import source must be codex, claude-code, kimi-code, or grok-cli",
                false,
            )
        })
}

pub(crate) fn oauth_import_path(source: &str) -> Result<PathBuf, HaiderError> {
    let spec = oauth_import_source_spec(source)?;
    oauth_import_path_for_spec(spec)
}

fn oauth_import_path_for_spec(spec: OAuthImportSourceSpec) -> Result<PathBuf, HaiderError> {
    if let Some(path) = std::env::var_os(spec.env_override).filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    let Some(home) = oauth_home_dir() else {
        return Err(HaiderError::new(
            ErrorCode::CredentialMissing,
            format!(
                "cannot locate OAuth import source `{}`: the home directory and {} are unset",
                spec.source, spec.env_override
            ),
            false,
        ));
    };
    Ok(PathBuf::from(home).join(spec.home_relative_path))
}

/// Returns the complete daemon-owned import catalog. Availability is sampled
/// during this call. Available sources sort first; both groups retain the
/// declaration order in [`OAUTH_IMPORT_SOURCE_SPECS`].
pub(crate) fn oauth_import_source_catalog(
    claude_native: &dyn ClaudeNativeCredentialStore,
) -> Vec<OAuthImportSourceWire> {
    let mut sources = OAUTH_IMPORT_SOURCE_SPECS
        .iter()
        .copied()
        .map(|spec| {
            if spec.source == "claude-code" {
                match claude_native.probe() {
                    Ok(()) => return available_oauth_import_source(spec),
                    Err(ClaudeNativeCredentialFailure::Missing) => {}
                    Err(_) => {
                        return unavailable_oauth_import_source(
                            spec,
                            OAuthImportSourceUnavailableCodeWire::Unreadable,
                        );
                    }
                }
            }
            oauth_import_source_file_entry(spec, oauth_import_path_for_spec(spec).ok())
        })
        .collect::<Vec<_>>();
    sources.sort_by_key(|source| !source.available);
    sources
}

fn oauth_import_source_file_entry(
    spec: OAuthImportSourceSpec,
    path: Option<PathBuf>,
) -> OAuthImportSourceWire {
    let Some(path) = path else {
        return unavailable_oauth_import_source(
            spec,
            OAuthImportSourceUnavailableCodeWire::NotFound,
        );
    };
    let readable = std::fs::File::open(path).and_then(|file| {
        if file.metadata()?.is_file() {
            Ok(())
        } else {
            Err(std::io::Error::other(
                "credential store is not a regular file",
            ))
        }
    });
    match readable {
        Ok(()) => available_oauth_import_source(spec),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            unavailable_oauth_import_source(spec, OAuthImportSourceUnavailableCodeWire::NotFound)
        }
        Err(_) => {
            unavailable_oauth_import_source(spec, OAuthImportSourceUnavailableCodeWire::Unreadable)
        }
    }
}

fn available_oauth_import_source(spec: OAuthImportSourceSpec) -> OAuthImportSourceWire {
    OAuthImportSourceWire {
        source: spec.source.to_owned(),
        provider: spec.provider.to_owned(),
        default_alias: spec.default_alias.to_owned(),
        available: true,
        unavailable_reason: None,
    }
}

fn unavailable_oauth_import_source(
    spec: OAuthImportSourceSpec,
    code: OAuthImportSourceUnavailableCodeWire,
) -> OAuthImportSourceWire {
    let message = match code {
        OAuthImportSourceUnavailableCodeWire::NotFound => format!(
            "No credentials were found for OAuth import source `{}`; sign in with that CLI and refresh.",
            spec.source
        ),
        OAuthImportSourceUnavailableCodeWire::Unreadable => format!(
            "Credentials for OAuth import source `{}` are present but the daemon cannot read them; check access permissions and refresh.",
            spec.source
        ),
        _ => {
            "OAuth import credentials are currently unavailable; refresh and try again.".to_owned()
        }
    };
    OAuthImportSourceWire {
        source: spec.source.to_owned(),
        provider: spec.provider.to_owned(),
        default_alias: spec.default_alias.to_owned(),
        available: false,
        unavailable_reason: Some(OAuthImportSourceUnavailableReasonWire { code, message }),
    }
}

pub(crate) fn oauth_home_dir() -> Option<std::ffi::OsString> {
    #[cfg(target_os = "windows")]
    let names = ["USERPROFILE", "HOME"];
    #[cfg(not(target_os = "windows"))]
    let names = ["HOME"];
    names
        .into_iter()
        .find_map(|name| std::env::var_os(name).filter(|value| !value.is_empty()))
}

#[derive(Clone)]
pub(crate) struct ClaudeCredentialInput {
    pub location: PathBuf,
    pub bytes: Zeroizing<Vec<u8>>,
    /// True only when the bytes came from Claude Code's live native owner
    /// store (Keychain/Credential Manager), never from an imported file.
    pub native_owner: bool,
}

/// Typed failures from Claude Code's native credential owner. These stay at
/// the injectable seam so daemon policy can distinguish an absent item from
/// a denied or locked Keychain without tests ever touching the real store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClaudeNativeCredentialFailure {
    // Unsupported production targets can only report `Missing`, but the
    // injected seam keeps these typed outcomes for supported targets/tests.
    #[cfg_attr(
        all(not(test), not(any(target_os = "macos", target_os = "windows"))),
        allow(dead_code)
    )]
    Denied,
    #[cfg_attr(
        all(not(test), not(any(target_os = "macos", target_os = "windows"))),
        allow(dead_code)
    )]
    Locked,
    Missing,
    #[cfg_attr(
        all(not(test), not(any(target_os = "macos", target_os = "windows"))),
        allow(dead_code)
    )]
    Unavailable,
}

/// Explicit-import reads may retry a previously failed no-UI probe and, on
/// macOS, request protected data interactively. Ordinary provider read-throughs
/// and adoption discovery never permit credential UI; the latter remains a
/// distinct event so one successful no-UI read can feed candidate lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClaudeNativeReadEvent {
    Ordinary,
    /// Explicit user-issued import. This is the only event permitted to ask
    /// macOS Keychain for protected credential data.
    Significant,
    AdoptionDiscovery,
}

#[cfg(any(test, target_os = "macos"))]
impl ClaudeNativeReadEvent {
    #[must_use]
    const fn credential_interaction_resolution(self) -> haider_core::InteractionResolution {
        let mode = if matches!(self, Self::Significant) {
            haider_protocol::session::SessionInteractionModeV1::Interactive
        } else {
            haider_protocol::session::SessionInteractionModeV1::Autonomous
        };
        haider_core::InteractionResolutionPolicy::new(mode)
            .resolve(haider_core::InteractionGate::CredentialOrLogin)
    }

    #[must_use]
    const fn macos_keychain_query_plan(self) -> MacosKeychainQueryPlan {
        MacosKeychainQueryPlan {
            skip_authenticated_attribute_items: true,
            skip_authenticated_data_items: true,
            allow_interactive_data_fallback: matches!(
                self.credential_interaction_resolution(),
                haider_core::InteractionResolution::AwaitHuman
            ),
        }
    }
}

/// Mechanically testable Keychain query contract. Every discovery/preflight
/// query skips protected items; only an explicit import may perform the later
/// interactive data read.
#[cfg(any(test, target_os = "macos"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MacosKeychainQueryPlan {
    skip_authenticated_attribute_items: bool,
    skip_authenticated_data_items: bool,
    allow_interactive_data_fallback: bool,
}

pub(crate) trait ClaudeNativeCredentialStore: Send + Sync {
    fn read(
        &self,
        event: ClaudeNativeReadEvent,
    ) -> Result<ClaudeCredentialInput, ClaudeNativeCredentialFailure>;

    /// Non-interactive availability probe. Implementations must not display
    /// an OS credential prompt; catalog refreshes are observational reads.
    fn probe(&self) -> Result<(), ClaudeNativeCredentialFailure> {
        self.read(ClaudeNativeReadEvent::Ordinary).map(drop)
    }
}

/// Raw platform store. The macOS implementation performs a no-UI query first
/// and permits at most one interactive Keychain read for this object (one
/// object is constructed per daemon boot).
#[derive(Default)]
pub(crate) struct PlatformClaudeNativeCredentialStore {
    #[cfg(all(target_os = "macos", not(test)))]
    interactive_attempted: AtomicBool,
    #[cfg(all(target_os = "macos", not(test)))]
    last_failure: Mutex<Option<ClaudeNativeCredentialFailure>>,
}

impl ClaudeNativeCredentialStore for PlatformClaudeNativeCredentialStore {
    fn read(
        &self,
        event: ClaudeNativeReadEvent,
    ) -> Result<ClaudeCredentialInput, ClaudeNativeCredentialFailure> {
        platform_claude_credential(self, event)
    }

    fn probe(&self) -> Result<(), ClaudeNativeCredentialFailure> {
        platform_claude_credential_probe(self)
    }
}

/// Boot-scoped single-flight/cooldown wrapper over the injected raw seam.
/// The account actor serializes callers, while this mutex also protects the
/// old discovery RPC's blocking worker and makes the no-second-call law local
/// to the seam rather than dependent on actor scheduling.
pub(crate) struct ClaudeNativeCredentialAccess {
    store: Arc<dyn ClaudeNativeCredentialStore>,
    state: Mutex<ClaudeNativeCredentialAccessState>,
}

#[derive(Default)]
struct ClaudeNativeCredentialAccessState {
    last_failure: Option<ClaudeNativeCredentialFailure>,
    /// A metadata discovery read and its immediate candidate lookup are one
    /// observational operation. Hand the bytes to the lookup once; explicit
    /// import still owns its separate `Significant` read.
    discovery_handoff: Option<(Instant, ClaudeCredentialInput)>,
}

impl ClaudeNativeCredentialAccess {
    const DISCOVERY_HANDOFF_TTL: Duration = Duration::from_secs(5);

    pub(crate) fn new(store: Arc<dyn ClaudeNativeCredentialStore>) -> Self {
        Self {
            store,
            state: Mutex::new(ClaudeNativeCredentialAccessState::default()),
        }
    }
}

impl ClaudeNativeCredentialStore for ClaudeNativeCredentialAccess {
    fn read(
        &self,
        event: ClaudeNativeReadEvent,
    ) -> Result<ClaudeCredentialInput, ClaudeNativeCredentialFailure> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if event == ClaudeNativeReadEvent::Ordinary {
            if let Some((observed_at, input)) = state.discovery_handoff.take()
                && observed_at.elapsed() <= Self::DISCOVERY_HANDOFF_TTL
            {
                return Ok(input);
            }
            if let Some(cached) = state.last_failure {
                return Err(cached);
            }
        }
        match self.store.read(event) {
            Ok(input) => {
                state.last_failure = None;
                state.discovery_handoff = (event == ClaudeNativeReadEvent::AdoptionDiscovery)
                    .then(|| (Instant::now(), input.clone()));
                Ok(input)
            }
            Err(error) => {
                state.discovery_handoff = None;
                state.last_failure = Some(error);
                Err(error)
            }
        }
    }

    fn probe(&self) -> Result<(), ClaudeNativeCredentialFailure> {
        self.store.probe()
    }
}

#[cfg(all(target_os = "macos", not(test)))]
fn platform_claude_credential_probe(
    _store: &PlatformClaudeNativeCredentialStore,
) -> Result<(), ClaudeNativeCredentialFailure> {
    use security_framework::item::{ItemClass, ItemSearchOptions, SearchResult};

    let plan = ClaudeNativeReadEvent::Ordinary.macos_keychain_query_plan();

    ItemSearchOptions::new()
        .class(ItemClass::generic_password())
        .service(CLAUDE_CODE_CREDENTIAL_SERVICE)
        .load_attributes(true)
        .skip_authenticated_items(plan.skip_authenticated_attribute_items)
        .search()
        .map_err(|error| classify_macos_keychain_status(error.code()))?;

    match ItemSearchOptions::new()
        .class(ItemClass::generic_password())
        .service(CLAUDE_CODE_CREDENTIAL_SERVICE)
        .load_data(true)
        .skip_authenticated_items(plan.skip_authenticated_data_items)
        .search()
    {
        Ok(results) => {
            if results
                .into_iter()
                .any(|item| matches!(item, SearchResult::Data(_)))
            {
                Ok(())
            } else {
                Err(ClaudeNativeCredentialFailure::Denied)
            }
        }
        Err(error) => Err(classify_macos_keychain_status(error.code())),
    }
}

#[cfg(all(target_os = "windows", not(test)))]
fn platform_claude_credential_probe(
    store: &PlatformClaudeNativeCredentialStore,
) -> Result<(), ClaudeNativeCredentialFailure> {
    platform_claude_credential(store, ClaudeNativeReadEvent::Ordinary).map(drop)
}

#[cfg(any(test, not(any(target_os = "macos", target_os = "windows"))))]
fn platform_claude_credential_probe(
    _store: &PlatformClaudeNativeCredentialStore,
) -> Result<(), ClaudeNativeCredentialFailure> {
    Err(ClaudeNativeCredentialFailure::Missing)
}

#[cfg(all(target_os = "macos", not(test)))]
fn platform_claude_credential(
    store: &PlatformClaudeNativeCredentialStore,
    event: ClaudeNativeReadEvent,
) -> Result<ClaudeCredentialInput, ClaudeNativeCredentialFailure> {
    use security_framework::item::{ItemClass, ItemSearchOptions, SearchResult};

    let plan = event.macos_keychain_query_plan();

    // Every discovery query opts out of Keychain authentication UI. A
    // protected item is therefore treated as unavailable unless this is the
    // explicit-import path below.
    let attribute_no_ui_failure = ItemSearchOptions::new()
        .class(ItemClass::generic_password())
        .service(CLAUDE_CODE_CREDENTIAL_SERVICE)
        .load_attributes(true)
        .skip_authenticated_items(plan.skip_authenticated_attribute_items)
        .search()
        .err()
        .map(|error| classify_macos_keychain_status(error.code()));

    let no_ui = ItemSearchOptions::new()
        .class(ItemClass::generic_password())
        .service(CLAUDE_CODE_CREDENTIAL_SERVICE)
        .load_data(true)
        .skip_authenticated_items(plan.skip_authenticated_data_items)
        .search();
    let no_ui_failure = no_ui
        .as_ref()
        .err()
        .map(|error| classify_macos_keychain_status(error.code()));
    if let Ok(results) = no_ui
        && let Some(bytes) = results.into_iter().find_map(|item| match item {
            SearchResult::Data(bytes) => Some(bytes),
            _ => None,
        })
    {
        *store
            .last_failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        return native_claude_input("macOS Keychain", Zeroizing::new(bytes));
    }

    if !plan.allow_interactive_data_fallback {
        return Err(no_ui_failure
            .or(attribute_no_ui_failure)
            .unwrap_or(ClaudeNativeCredentialFailure::Denied));
    }

    // Protected data needs UI. Only an explicit import may reach this branch,
    // and only the first attempt in this daemon boot is allowed to ask.
    if store.interactive_attempted.swap(true, Ordering::AcqRel) {
        return Err(store
            .last_failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .unwrap_or(ClaudeNativeCredentialFailure::Denied));
    }
    let result = ItemSearchOptions::new()
        .class(ItemClass::generic_password())
        .service(CLAUDE_CODE_CREDENTIAL_SERVICE)
        .load_data(true)
        .search()
        .and_then(|items| {
            items
                .into_iter()
                .find_map(|item| match item {
                    SearchResult::Data(bytes) => Some(bytes),
                    _ => None,
                })
                .ok_or_else(|| security_framework::base::Error::from_code(-25300))
        });
    match result {
        Ok(bytes) => {
            *store
                .last_failure
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            native_claude_input("macOS Keychain", Zeroizing::new(bytes))
        }
        Err(error) => {
            let failure = classify_macos_keychain_status(error.code());
            *store
                .last_failure
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(failure);
            Err(failure)
        }
    }
}

#[cfg(all(target_os = "macos", not(test)))]
fn classify_macos_keychain_status(status: i32) -> ClaudeNativeCredentialFailure {
    match status {
        -128 | -25243 => ClaudeNativeCredentialFailure::Denied,
        -25293 => ClaudeNativeCredentialFailure::Locked,
        -25300 => ClaudeNativeCredentialFailure::Missing,
        -25291 | -25308 | -25315 => ClaudeNativeCredentialFailure::Unavailable,
        _ => ClaudeNativeCredentialFailure::Unavailable,
    }
}

#[cfg(all(target_os = "windows", not(test)))]
fn platform_claude_credential(
    _store: &PlatformClaudeNativeCredentialStore,
    _event: ClaudeNativeReadEvent,
) -> Result<ClaudeCredentialInput, ClaudeNativeCredentialFailure> {
    let bytes = windows_claude_store::read()?;
    native_claude_input(
        "Windows Credential Manager",
        normalize_windows_credential(bytes).ok_or(ClaudeNativeCredentialFailure::Unavailable)?,
    )
}

#[cfg(all(target_os = "windows", not(test)))]
fn normalize_windows_credential(mut bytes: Zeroizing<Vec<u8>>) -> Option<Zeroizing<Vec<u8>>> {
    if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        bytes.drain(..3);
    }
    let utf8_end = bytes.iter().rposition(|byte| *byte != 0)? + 1;
    if serde_json::from_slice::<serde_json::Value>(&bytes[..utf8_end]).is_ok() {
        bytes.truncate(utf8_end);
        return Some(bytes);
    }
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let big_endian = bytes.starts_with(&[0xfe, 0xff]);
    let little_endian = bytes.starts_with(&[0xff, 0xfe]);
    let start = usize::from(big_endian || little_endian) * 2;
    // Zeroizing has no FromIterator — collect first, then wrap (same
    // zeroize-on-drop guarantee).
    let mut units = Zeroizing::new(
        bytes[start..]
            .chunks_exact(2)
            .map(|unit| {
                if big_endian {
                    u16::from_be_bytes([unit[0], unit[1]])
                } else {
                    u16::from_le_bytes([unit[0], unit[1]])
                }
            })
            .collect::<Vec<_>>(),
    );
    while units.last() == Some(&0) {
        units.pop();
    }
    let mut decoded = Zeroizing::new(Vec::new());
    for character in char::decode_utf16(units.iter().copied()) {
        let character = character.ok()?;
        let mut encoded = [0; 4];
        decoded.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
    }
    serde_json::from_slice::<serde_json::Value>(&decoded)
        .is_ok()
        .then_some(decoded)
}

/// Minimal WinCred adapter for Claude Code's generic credential. Platform
/// failures stay typed at the injectable caller-facing seam.
#[cfg(all(target_os = "windows", not(test)))]
#[allow(unsafe_code)]
mod windows_claude_store {
    use std::ffi::c_void;
    use std::ptr;
    use std::slice;

    use windows_sys::Win32::Security::Credentials::{
        CRED_TYPE_GENERIC, CREDENTIALW, CredFree, CredReadW,
    };
    use zeroize::Zeroizing;

    use super::CLAUDE_CODE_CREDENTIAL_SERVICE;

    struct CredentialGuard(*mut CREDENTIALW);

    impl Drop for CredentialGuard {
        fn drop(&mut self) {
            unsafe { CredFree(self.0.cast::<c_void>()) };
        }
    }

    pub(super) fn read() -> Result<Zeroizing<Vec<u8>>, super::ClaudeNativeCredentialFailure> {
        use windows_sys::Win32::Foundation::{
            ERROR_ACCESS_DENIED, ERROR_LOGON_FAILURE, ERROR_NO_SUCH_LOGON_SESSION, ERROR_NOT_FOUND,
            GetLastError,
        };

        // Claude Code's generic-credential target is the same stable string as
        // its macOS Keychain service name.
        let target = CLAUDE_CODE_CREDENTIAL_SERVICE
            .encode_utf16()
            .chain([0])
            .collect::<Vec<_>>();
        let mut credential = ptr::null_mut::<CREDENTIALW>();
        let found = unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) };
        if found == 0 || credential.is_null() {
            return Err(match unsafe { GetLastError() } {
                ERROR_NOT_FOUND => super::ClaudeNativeCredentialFailure::Missing,
                ERROR_ACCESS_DENIED => super::ClaudeNativeCredentialFailure::Denied,
                ERROR_LOGON_FAILURE | ERROR_NO_SUCH_LOGON_SESSION => {
                    super::ClaudeNativeCredentialFailure::Locked
                }
                _ => super::ClaudeNativeCredentialFailure::Unavailable,
            });
        }
        let guard = CredentialGuard(credential);
        let size = unsafe { (*guard.0).CredentialBlobSize as usize };
        let blob = unsafe { (*guard.0).CredentialBlob };
        if size == 0 || blob.is_null() {
            return Err(super::ClaudeNativeCredentialFailure::Unavailable);
        }
        Ok(Zeroizing::new(
            unsafe { slice::from_raw_parts(blob, size) }.to_vec(),
        ))
    }
}

#[cfg(any(test, not(any(target_os = "macos", target_os = "windows"))))]
fn platform_claude_credential(
    _store: &PlatformClaudeNativeCredentialStore,
    _event: ClaudeNativeReadEvent,
) -> Result<ClaudeCredentialInput, ClaudeNativeCredentialFailure> {
    Err(ClaudeNativeCredentialFailure::Missing)
}

#[cfg(all(not(test), any(target_os = "macos", target_os = "windows")))]
fn native_claude_input(
    store: &str,
    bytes: Zeroizing<Vec<u8>>,
) -> Result<ClaudeCredentialInput, ClaudeNativeCredentialFailure> {
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > IMPORT_FILE_LIMIT {
        return Err(ClaudeNativeCredentialFailure::Unavailable);
    }
    Ok(ClaudeCredentialInput {
        location: PathBuf::from(format!("{store}: {CLAUDE_CODE_CREDENTIAL_SERVICE}")),
        bytes,
        native_owner: true,
    })
}

pub(crate) fn load_claude_credential_input(
    file_path: &Path,
    native: &dyn ClaudeNativeCredentialStore,
    event: ClaudeNativeReadEvent,
) -> Result<ClaudeCredentialInput, HaiderError> {
    match native.read(event) {
        Ok(mut input) => {
            input.native_owner = true;
            return Ok(input);
        }
        Err(ClaudeNativeCredentialFailure::Missing) => {}
        Err(failure) => return Err(claude_native_access_error(failure)),
    }
    match std::fs::File::open(file_path) {
        Ok(file) => read_oauth_import_reader(file_path, "claude-code", file),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(HaiderError::new(
            ErrorCode::CredentialMissing,
            format!(
                "cannot find Claude Code credentials at `{}` or in the native secure store",
                file_path.display()
            ),
            false,
        )),
        Err(error) => Err(oauth_import_read_error(file_path, "claude-code", &error)),
    }
}

pub(crate) struct OAuthImportMaterial {
    pub bundle: OAuthTokenBundleV1,
    /// One-way digest of the exact source-store bytes parsed into `bundle`.
    /// Used only to bind an explicit device-candidate confirmation.
    pub source_fingerprint: [u8; 32],
    pub kimi_device_id: Option<Zeroizing<Vec<u8>>>,
    /// Claude Code remains the credential owner, so expiry resolution must
    /// read through to its native store and must not spend Haider's snapshot.
    pub claude_native_owner: bool,
}

/// Reads and converts one daemon-local CLI credential file.
///
/// The returned material is the first and only object allowed to leave this
/// function. File bytes and all token fields stay in zeroizing storage.
#[allow(dead_code)]
pub(crate) fn load_oauth_import_material(
    source: &str,
    generation: u64,
) -> Result<OAuthImportMaterial, HaiderError> {
    load_oauth_import_material_with_native(
        source,
        generation,
        &PlatformClaudeNativeCredentialStore::default(),
        ClaudeNativeReadEvent::Significant,
    )
}

pub(crate) fn load_oauth_import_material_with_native(
    source: &str,
    generation: u64,
    native: &dyn ClaudeNativeCredentialStore,
    event: ClaudeNativeReadEvent,
) -> Result<OAuthImportMaterial, HaiderError> {
    let spec = oauth_import_source_spec(source)?;
    let path = oauth_import_path(source)?;
    let input = if spec.source == "claude-code" {
        load_claude_credential_input(&path, native, event)?
    } else {
        let bytes = read_oauth_import_file(&path, spec.source)?;
        ClaudeCredentialInput {
            location: path,
            bytes,
            native_owner: false,
        }
    };
    load_oauth_import_material_from_input(spec, generation, input)
}

/// Reads Claude Code's live owner store directly, ahead of any imported
/// credential file. `None` is the sole signal that the independent grant
/// fallback is eligible for a file-only credential.
#[derive(Debug)]
pub(crate) enum ClaudeNativeImportError {
    Access(ClaudeNativeCredentialFailure),
    Invalid(HaiderError),
}

pub(crate) fn load_claude_native_import_material(
    generation: u64,
    native: &dyn ClaudeNativeCredentialStore,
    event: ClaudeNativeReadEvent,
) -> Result<OAuthImportMaterial, ClaudeNativeImportError> {
    let mut input = native
        .read(event)
        .map_err(ClaudeNativeImportError::Access)?;
    input.native_owner = true;
    let spec = oauth_import_source_spec("claude-code").map_err(ClaudeNativeImportError::Invalid)?;
    load_oauth_import_material_from_input(spec, generation, input)
        .map_err(ClaudeNativeImportError::Invalid)
}

pub(crate) fn claude_native_access_error(failure: ClaudeNativeCredentialFailure) -> HaiderError {
    let message = match failure {
        ClaudeNativeCredentialFailure::Denied => {
            "Claude Code credential access was denied; re-allow Keychain access or re-link"
        }
        ClaudeNativeCredentialFailure::Locked => {
            "Claude Code Keychain is locked; unlock the login keychain (its password may have changed)"
        }
        ClaudeNativeCredentialFailure::Missing => {
            "Claude Code credential is missing; re-link the account"
        }
        ClaudeNativeCredentialFailure::Unavailable => {
            "Claude Code credential store is unavailable; retry after the system store recovers"
        }
    };
    HaiderError::new(ErrorCode::CredentialMissing, message, true)
}

fn load_oauth_import_material_from_input(
    spec: OAuthImportSourceSpec,
    generation: u64,
    input: ClaudeCredentialInput,
) -> Result<OAuthImportMaterial, HaiderError> {
    let claude_native_owner = input.native_owner;
    let path = input.location;
    let bytes = input.bytes;
    let source_fingerprint = *blake3::hash(&bytes).as_bytes();
    let registration = OAuthProviderCatalog::default()
        .registration(spec.provider)
        .ok_or_else(|| {
            HaiderError::new(
                ErrorCode::Unauthorized,
                format!(
                    "OAuth import source `{}` has no sanctioned provider registration",
                    spec.source
                ),
                false,
            )
        })?;
    let mut bundle = match spec.source {
        "codex" => codex_import_bundle(&path, &bytes, &registration, generation),
        "claude-code" => claude_import_bundle(&path, &bytes, &registration, generation),
        "kimi-code" => kimi_import_bundle(&path, &bytes, &registration, generation),
        "grok-cli" => grok_import_bundle(&path, &bytes, &registration, generation),
        _ => unreachable!("source spec is closed"),
    }?;
    match spec.source {
        "codex" => bundle.identity.display_identity.push_str(" · Codex"),
        "claude-code" => bundle
            .identity
            .display_identity
            .push_str(if claude_native_owner {
                " · Linked to Claude Code"
            } else {
                " · independently imported"
            }),
        "kimi-code" => bundle.identity.display_identity.push_str(" · Kimi Code"),
        "grok-cli" => bundle.identity.display_identity.push_str(" · Grok CLI"),
        _ => {}
    }
    let kimi_device_id = if spec.source == "kimi-code" {
        let device_path = crate::device_discovery::kimi_device_id_path().ok_or_else(|| {
            HaiderError::new(
                ErrorCode::CredentialMissing,
                "cannot locate Kimi Code device identity: HOME and HAIDER_KIMI_DEVICE_ID_PATH are unset",
                false,
            )
        })?;
        let device = read_oauth_import_file(&device_path, "kimi-code device identity")?;
        let trimmed = crate::device_discovery::trim_ascii(&device);
        if !crate::device_discovery::valid_kimi_device_id(trimmed) {
            return Err(invalid_import(&device_path, "kimi-code device identity"));
        }
        Some(Zeroizing::new(trimmed.to_vec()))
    } else {
        None
    };
    Ok(OAuthImportMaterial {
        bundle,
        source_fingerprint,
        kimi_device_id,
        claude_native_owner,
    })
}

fn read_oauth_import_file(path: &Path, source: &str) -> Result<Zeroizing<Vec<u8>>, HaiderError> {
    #[cfg(test)]
    OAUTH_IMPORT_READ_COUNT.fetch_add(1, Ordering::SeqCst);
    let file =
        std::fs::File::open(path).map_err(|error| oauth_import_read_error(path, source, &error))?;
    read_oauth_import_reader(path, source, file).map(|input| input.bytes)
}

fn read_oauth_import_reader(
    path: &Path,
    source: &str,
    file: std::fs::File,
) -> Result<ClaudeCredentialInput, HaiderError> {
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(IMPORT_FILE_LIMIT.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| oauth_import_read_error(path, source, &error))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > IMPORT_FILE_LIMIT {
        return Err(HaiderError::new(
            ErrorCode::InvalidArgument,
            format!(
                "OAuth import source `{source}` at `{}` is too large",
                path.display()
            ),
            false,
        ));
    }
    Ok(ClaudeCredentialInput {
        location: path.to_owned(),
        bytes,
        native_owner: false,
    })
}

fn oauth_import_read_error(path: &Path, source: &str, error: &std::io::Error) -> HaiderError {
    HaiderError::new(
        ErrorCode::CredentialMissing,
        format!(
            "cannot read OAuth import source `{source}` at `{}`: {error}",
            path.display()
        ),
        false,
    )
}

#[derive(Deserialize)]
struct CodexAuthFile {
    tokens: CodexTokens,
}

#[derive(Deserialize)]
struct CodexTokens {
    #[serde(default)]
    id_token: Option<SecretJson>,
    access_token: SecretJson,
    refresh_token: SecretJson,
    #[serde(default)]
    account_id: Option<String>,
}

#[derive(Default, Deserialize)]
struct ImportedJwtClaims {
    #[serde(default)]
    exp: Option<u64>,
    #[serde(default)]
    sub: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    chatgpt_account_id: Option<String>,
    #[serde(default, rename = "https://api.openai.com/auth")]
    openai_auth: Option<ImportedOpenAiAuthClaims>,
}

#[derive(Default, Deserialize)]
struct ImportedOpenAiAuthClaims {
    #[serde(default)]
    chatgpt_account_id: Option<String>,
}

fn codex_import_bundle(
    path: &Path,
    bytes: &[u8],
    registration: &OAuthProviderRegistration,
    generation: u64,
) -> Result<OAuthTokenBundleV1, HaiderError> {
    let auth: CodexAuthFile =
        serde_json::from_slice(bytes).map_err(|error| malformed_import(path, "codex", &error))?;
    let captured_at = now_ms().ok_or_else(|| {
        HaiderError::new(
            ErrorCode::Internal,
            "cannot timestamp imported OAuth identity",
            true,
        )
    })?;
    let account_identity = haider_provider::oauth_identity_source(&registration.provider_id)
        .and_then(|source| {
            source
                .identity_from_tokens(&haider_provider::OAuthTokens {
                    access_token: auth.tokens.access_token.0.as_slice(),
                    refresh_token: Some(auth.tokens.refresh_token.0.as_slice()),
                    id_token: auth
                        .tokens
                        .id_token
                        .as_ref()
                        .map(|token| token.0.as_slice()),
                    captured_at,
                })
                .ok()
                .flatten()
        });
    let source_access_fingerprint = *blake3::hash(auth.tokens.access_token.0.as_slice()).as_bytes();
    let parsed_expiry = unverified_jwt_expiry_ms(auth.tokens.access_token.0.as_slice());
    let refresh_on_first_use = parsed_expiry.is_none();
    let expires_at_unix_ms = parsed_expiry
        .or_else(|| {
            now_ms().and_then(|now| now.checked_add(duration_ms(CODEX_IMPORT_FALLBACK_WINDOW)))
        })
        .ok_or_else(|| {
            HaiderError::new(
                ErrorCode::Internal,
                "cannot stamp fallback OAuth import expiry",
                true,
            )
        })?;
    let identity_claims = auth
        .tokens
        .id_token
        .as_ref()
        .and_then(|token| decode_unverified_jwt_payload::<ImportedJwtClaims>(token.0.as_slice()))
        .unwrap_or_default();
    let email = identity_claims.email.and_then(nonempty);
    let account_id = identity_claims
        .chatgpt_account_id
        .and_then(nonempty)
        .or_else(|| {
            identity_claims
                .openai_auth
                .and_then(|claims| claims.chatgpt_account_id.and_then(nonempty))
        })
        .or_else(|| auth.tokens.account_id.and_then(nonempty));
    let display_identity = email
        .clone()
        .or_else(|| account_id.clone())
        .unwrap_or_else(|| "imported".to_owned());
    let subject = identity_claims
        .sub
        .and_then(nonempty)
        .or_else(|| account_id.clone())
        .unwrap_or_else(|| "imported".to_owned());
    let account_identity = account_identity.or_else(|| {
        (email.is_some() || account_id.is_some()).then(|| AccountIdentity {
            email,
            display_name: None,
            account_id,
            plan: None,
            issuer: Some(registration.issuer.clone()),
            captured_at,
            verified: false,
        })
    });
    let id_token = auth.tokens.id_token.map(|token| token.0);
    let mut bundle = OAuthTokenBundleV1::new(
        registration.provider_id.clone(),
        registration.issuer.clone(),
        registration.audience.clone(),
        registration.resource.clone(),
        "Bearer".into(),
        auth.tokens.access_token.0,
        Some(auth.tokens.refresh_token.0),
        expires_at_unix_ms,
        None,
        registration.scopes.clone(),
        OAuthIdentityV1 {
            subject_hash: blake3::hash(subject.as_bytes()).to_hex().to_string(),
            display_identity,
        },
        generation,
    )
    .map_err(|_| invalid_import(path, "codex"))?;
    if let Some(identity) = account_identity {
        bundle = bundle.with_account_identity(identity);
    }
    if let Some(id_token) = id_token {
        bundle = bundle.with_id_token(id_token);
    }
    let bundle = bundle.with_import_source_access_fingerprint(source_access_fingerprint);
    Ok(if refresh_on_first_use {
        bundle.with_refresh_on_first_use()
    } else {
        bundle
    })
}

#[derive(Deserialize)]
struct ClaudeCredentialsFile {
    #[serde(rename = "claudeAiOauth")]
    oauth: ClaudeCredentials,
}

#[derive(Deserialize)]
struct ClaudeCredentials {
    #[serde(rename = "accessToken")]
    access_token: SecretJson,
    #[serde(rename = "refreshToken")]
    refresh_token: SecretJson,
    #[serde(rename = "expiresAt")]
    expires_at_unix_ms: u64,
    #[serde(default, rename = "refreshTokenExpiresAt")]
    refresh_expires_at_unix_ms: Option<u64>,
    scopes: Vec<String>,
    #[serde(default, rename = "subscriptionType")]
    subscription_type: Option<String>,
    #[serde(default, rename = "clientId")]
    client_id: Option<String>,
}

fn claude_import_bundle(
    path: &Path,
    bytes: &[u8],
    registration: &OAuthProviderRegistration,
    generation: u64,
) -> Result<OAuthTokenBundleV1, HaiderError> {
    let credentials = parse_claude_credentials(path, bytes)?;
    let source_access_fingerprint =
        *blake3::hash(credentials.oauth.access_token.0.as_slice()).as_bytes();
    if credentials.oauth.access_token.0.is_empty()
        || credentials.oauth.refresh_token.0.is_empty()
        || credentials.oauth.expires_at_unix_ms == 0
        || credentials
            .oauth
            .refresh_expires_at_unix_ms
            .is_some_and(|expiry| expiry == 0)
        || credentials
            .oauth
            .client_id
            .as_deref()
            .is_some_and(|client_id| client_id != registration.client_id)
    {
        return Err(invalid_import(path, "claude-code"));
    }
    if !registration
        .scopes
        .iter()
        .filter(|scope| registration.validation_required(scope))
        .all(|scope| credentials.oauth.scopes.contains(scope))
    {
        return Err(invalid_import(path, "claude-code"));
    }
    let (display_identity, plan) =
        claude_subscription_identity(credentials.oauth.subscription_type.as_deref());
    let identity = OAuthIdentityV1 {
        subject_hash: blake3::hash(credentials.oauth.access_token.0.as_slice())
            .to_hex()
            .to_string(),
        display_identity: display_identity.into(),
    };
    let captured_at = now_ms().ok_or_else(|| {
        HaiderError::new(
            ErrorCode::Internal,
            "cannot timestamp imported OAuth identity",
            true,
        )
    })?;
    OAuthTokenBundleV1::new(
        registration.provider_id.clone(),
        registration.issuer.clone(),
        registration.audience.clone(),
        registration.resource.clone(),
        "Bearer".into(),
        credentials.oauth.access_token.0,
        Some(credentials.oauth.refresh_token.0),
        credentials.oauth.expires_at_unix_ms,
        credentials.oauth.refresh_expires_at_unix_ms,
        credentials.oauth.scopes,
        identity,
        generation,
    )
    .map(|bundle| {
        bundle
            .with_account_identity(AccountIdentity {
                email: None,
                display_name: Some(display_identity.to_owned()),
                account_id: None,
                plan: plan.map(str::to_owned),
                issuer: Some(registration.issuer.clone()),
                captured_at,
                verified: false,
            })
            .with_import_source_access_fingerprint(source_access_fingerprint)
    })
    .map_err(|_| invalid_import(path, "claude-code"))
}

pub(crate) fn claude_subscription_identity(
    subscription_type: Option<&str>,
) -> (&'static str, Option<&'static str>) {
    match subscription_type {
        Some("max") => ("Claude Max subscription", Some("max")),
        Some("pro") => ("Claude Pro subscription", Some("pro")),
        Some("team") => ("Claude Team subscription", Some("team")),
        Some("enterprise") => ("Claude Enterprise subscription", Some("enterprise")),
        _ => ("Claude Code subscription", None),
    }
}

fn parse_claude_credentials(
    path: &Path,
    bytes: &[u8],
) -> Result<ClaudeCredentialsFile, HaiderError> {
    serde_json::from_slice(bytes).map_err(|error| malformed_import(path, "claude-code", &error))
}

pub(crate) struct ClaudeCredentialMetadata {
    pub expires_at_ms: u64,
    pub has_inference_scope: bool,
    pub custom_client: bool,
    pub subscription_type: Option<String>,
}

pub(crate) fn parse_claude_credential_metadata(
    path: &Path,
    bytes: &[u8],
) -> Result<ClaudeCredentialMetadata, HaiderError> {
    let credentials = parse_claude_credentials(path, bytes)?;
    if credentials.oauth.access_token.0.is_empty()
        || credentials.oauth.refresh_token.0.is_empty()
        || credentials.oauth.expires_at_unix_ms == 0
    {
        return Err(invalid_import(path, "claude-code"));
    }
    Ok(ClaudeCredentialMetadata {
        expires_at_ms: credentials.oauth.expires_at_unix_ms,
        has_inference_scope: credentials
            .oauth
            .scopes
            .iter()
            .any(|scope| scope == "user:inference"),
        custom_client: credentials
            .oauth
            .client_id
            .as_deref()
            .is_some_and(|client_id| client_id != CLAUDE_DEFAULT_CLIENT_ID),
        subscription_type: credentials.oauth.subscription_type,
    })
}

#[derive(Deserialize)]
struct KimiCredentials {
    access_token: SecretJson,
    refresh_token: SecretJson,
    expires_at: f64,
    #[serde(default)]
    expires_in: f64,
    scope: String,
    token_type: String,
}

fn kimi_import_bundle(
    path: &Path,
    bytes: &[u8],
    registration: &OAuthProviderRegistration,
    generation: u64,
) -> Result<OAuthTokenBundleV1, HaiderError> {
    let credentials: KimiCredentials = serde_json::from_slice(bytes)
        .map_err(|error| malformed_import(path, "kimi-code", &error))?;
    if credentials.access_token.0.is_empty()
        || credentials.refresh_token.0.is_empty()
        || !credentials.token_type.eq_ignore_ascii_case("bearer")
        || !credentials.expires_at.is_finite()
        || credentials.expires_at <= 0.0
        || credentials.expires_at > (u64::MAX / 1000) as f64
        || !credentials.expires_in.is_finite()
        || credentials.expires_in.is_sign_negative()
    {
        return Err(invalid_import(path, "kimi-code"));
    }
    let expires_at_unix_ms = (credentials.expires_at * 1000.0) as u64;
    let expires_in = credentials.expires_in as u64;
    let refresh_threshold = if expires_in == 0 {
        300
    } else {
        300_u64.max(expires_in / 2)
    };
    let source_access_fingerprint = *blake3::hash(&credentials.access_token.0).as_bytes();
    OAuthTokenBundleV1::new(
        registration.provider_id.clone(),
        registration.issuer.clone(),
        registration.audience.clone(),
        registration.resource.clone(),
        credentials.token_type,
        credentials.access_token.0,
        Some(credentials.refresh_token.0),
        expires_at_unix_ms,
        None,
        credentials
            .scope
            .split_ascii_whitespace()
            .map(str::to_owned)
            .collect(),
        OAuthIdentityV1 {
            subject_hash: blake3::Hash::from_bytes(source_access_fingerprint)
                .to_hex()
                .to_string(),
            display_identity: "Kimi Code subscription".into(),
        },
        generation,
    )
    .map(|bundle| {
        bundle
            .with_import_source_access_fingerprint(source_access_fingerprint)
            .with_refresh_after(
                expires_at_unix_ms.saturating_sub(refresh_threshold.saturating_mul(1000)),
            )
    })
    .map_err(|_| invalid_import(path, "kimi-code"))
}

#[derive(Deserialize)]
#[serde(untagged)]
enum GrokCredentials {
    Bare(SecretJson),
    Bundle {
        access_token: SecretJson,
        #[serde(default)]
        refresh_token: Option<SecretJson>,
        #[serde(default)]
        expires_in: Option<u64>,
        #[serde(default)]
        issuer: Option<String>,
    },
}

/// Converts both official Grok CLI auth-store shapes without retaining the
/// source JSON. The bare-token legacy shape has no expiry metadata, so it is
/// deliberately short-lived and cannot refresh; a 401 then drives re-login.
fn grok_import_bundle(
    path: &Path,
    bytes: &[u8],
    registration: &OAuthProviderRegistration,
    generation: u64,
) -> Result<OAuthTokenBundleV1, HaiderError> {
    let credentials: GrokCredentials = serde_json::from_slice(bytes)
        .map_err(|error| malformed_import(path, "grok-cli", &error))?;
    let (access_token, refresh_token, expires_in, issuer) = match credentials {
        GrokCredentials::Bare(token) => (token.0, None, 15 * 60, None),
        GrokCredentials::Bundle {
            access_token,
            refresh_token,
            expires_in,
            issuer,
        } => (
            access_token.0,
            refresh_token.map(|token| token.0),
            expires_in.unwrap_or(60 * 60),
            issuer,
        ),
    };
    if access_token.is_empty()
        || refresh_token.as_ref().is_some_and(|token| token.is_empty())
        || expires_in == 0
        || expires_in > MAX_TOKEN_LIFETIME_SECS
        || issuer
            .as_deref()
            .is_some_and(|value| value != registration.issuer)
    {
        return Err(invalid_import(path, "grok-cli"));
    }
    let now = now_ms().ok_or_else(|| invalid_import(path, "grok-cli"))?;
    let expires_at_unix_ms = now
        .checked_add(expires_in.saturating_mul(1000))
        .ok_or_else(|| invalid_import(path, "grok-cli"))?;
    let source_access_fingerprint = *blake3::hash(&access_token).as_bytes();
    OAuthTokenBundleV1::new(
        registration.provider_id.clone(),
        registration.issuer.clone(),
        registration.audience.clone(),
        registration.resource.clone(),
        "Bearer".to_owned(),
        access_token,
        refresh_token,
        expires_at_unix_ms,
        None,
        registration.scopes.clone(),
        OAuthIdentityV1 {
            subject_hash: blake3::Hash::from_bytes(source_access_fingerprint)
                .to_hex()
                .to_string(),
            display_identity: "SuperGrok/X Premium subscription".into(),
        },
        generation,
    )
    .map(|bundle| bundle.with_import_source_access_fingerprint(source_access_fingerprint))
    .map_err(|_| invalid_import(path, "grok-cli"))
}

fn decode_unverified_jwt_payload<T>(token: &[u8]) -> Option<T>
where
    T: serde::de::DeserializeOwned,
{
    let token = std::str::from_utf8(token).ok()?;
    let mut segments = token.split('.');
    let _header = segments.next()?;
    let payload = segments.next()?;
    let _signature = segments.next()?;
    let mut decoded = Zeroizing::new(Vec::new());
    URL_SAFE_NO_PAD
        .decode_vec(payload.as_bytes(), &mut decoded)
        .ok()?;
    serde_json::from_slice(&decoded).ok()
}

fn unverified_jwt_expiry_ms(token: &[u8]) -> Option<u64> {
    decode_unverified_jwt_payload::<ImportedJwtClaims>(token)?
        .exp?
        .checked_mul(1000)
        .filter(|expiry| *expiry != 0)
}

fn codex_import_fallback_refresh_candidate(bundle: &OAuthTokenBundleV1, now: u64) -> bool {
    bundle.provider_id == haider_provider::OPENAI_OAUTH_PROVIDER_NAME
        && bundle.refresh_on_first_use()
        && bundle.expires_at_unix_ms
            <= now.saturating_add(duration_ms(CODEX_IMPORT_FALLBACK_WINDOW))
}

fn nonempty(value: String) -> Option<String> {
    AccountIdentity::sanitized_field(&value)
}

fn malformed_import(path: &Path, source: &str, error: &serde_json::Error) -> HaiderError {
    HaiderError::new(
        ErrorCode::InvalidArgument,
        format!(
            "OAuth import source `{source}` at `{}` is malformed JSON at line {}, column {}",
            path.display(),
            error.line(),
            error.column()
        ),
        false,
    )
}

fn invalid_import(path: &Path, source: &str) -> HaiderError {
    HaiderError::new(
        ErrorCode::InvalidArgument,
        format!(
            "OAuth import source `{source}` at `{}` has invalid credential fields",
            path.display()
        ),
        false,
    )
}

fn is_numeric_loopback_http(url: &Url) -> bool {
    url.scheme() == "http" && matches!(url.host(), Some(url::Host::Ipv4(ip)) if ip.is_loopback())
}

/// How long a started browser flow stays valid, from `start` to code
/// exchange. Generous by design: the user is off in a browser reading the
/// provider's consent page, possibly logging in and completing 2FA first.
/// The pre-fix tie to the 5-minute staged-secret TTL could expire the flow
/// mid-consent, tearing the listener down so the eventual callback found
/// nothing (or a reused port). Ten minutes is the flow's own law; the
/// rejection page quotes it.
pub(crate) const OAUTH_FLOW_TTL: Duration = Duration::from_secs(600);

/// Flow bounds and deadlines. Tests inject short deterministic values.
#[derive(Debug, Clone, Copy)]
pub struct OAuthCoordinatorConfig {
    pub max_flows: usize,
    pub max_invalid_callbacks: usize,
    pub flow_ttl: Duration,
}

impl Default for OAuthCoordinatorConfig {
    fn default() -> Self {
        Self {
            max_flows: 16,
            max_invalid_callbacks: 8,
            flow_ttl: OAUTH_FLOW_TTL,
        }
    }
}

pub(crate) struct OAuthRoute {
    pub request_id: RequestId,
    pub sink: Arc<dyn FrameSink>,
}

struct StartJob {
    owner: FlowOwner,
    provider: String,
    desired_alias: String,
    attempt_id: String,
    route: OAuthRoute,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FlowOwner {
    daemon_instance: String,
    connection_id: String,
}

enum InternalFlowStatus {
    WaitingBrowser,
    WaitingDevice,
    Exchanging,
    Ready {
        reference: Zeroizing<String>,
        bundle: Box<OAuthTokenBundleV1>,
    },
    Failed(&'static str),
    Expired,
    Cancelled,
}

struct FlowEntry {
    owner: FlowOwner,
    provider: String,
    desired_alias: String,
    attempt_id: String,
    deadline: Instant,
    expires_at_ms: u64,
    status: InternalFlowStatus,
    cancel: watch::Sender<bool>,
}

struct CoordinatorInner {
    instance_id: String,
    catalog: OAuthProviderCatalog,
    config: OAuthCoordinatorConfig,
    flows: Mutex<HashMap<String, FlowEntry>>,
    active_connections: Mutex<HashSet<String>>,
    shutting_down: AtomicBool,
    shutdown: watch::Sender<bool>,
    tasks: Weak<OwnedTaskSet>,
    next_flow: AtomicU64,
    client: reqwest::Client,
    vault: Arc<dyn Vault>,
}

impl Drop for CoordinatorInner {
    fn drop(&mut self) {
        if let Ok(mut flows) = self.flows.lock() {
            for (_, flow) in flows.drain() {
                flow.cancel.send_replace(true);
            }
        }
    }
}

/// Cloneable coordinator handle. Start is a bounded actor handoff; status,
/// cancel, claim, and disconnect cleanup are short mutex operations.
#[derive(Clone)]
pub(crate) struct OAuthCoordinator {
    inner: Arc<CoordinatorInner>,
    starts: mpsc::Sender<StartJob>,
    tasks: Arc<OwnedTaskSet>,
}

impl OAuthCoordinator {
    #[cfg(test)]
    pub(crate) fn new(
        instance_id: String,
        catalog: OAuthProviderCatalog,
        config: OAuthCoordinatorConfig,
    ) -> Result<Self, OAuthPublicError> {
        Self::new_with_vault(
            instance_id,
            catalog,
            config,
            Arc::new(haider_accounts::MemoryVault::default()),
        )
    }

    pub(crate) fn new_with_vault(
        instance_id: String,
        catalog: OAuthProviderCatalog,
        config: OAuthCoordinatorConfig,
        vault: Arc<dyn Vault>,
    ) -> Result<Self, OAuthPublicError> {
        if config.max_flows == 0 || config.max_invalid_callbacks == 0 {
            return Err(OAuthPublicError::new("invalid_oauth_limits", false));
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(TOKEN_TIMEOUT)
            .build()
            .map_err(|_| OAuthPublicError::new("oauth_transport_unavailable", true))?;
        let tasks = Arc::new(OwnedTaskSet::new());
        let inner = Arc::new(CoordinatorInner {
            instance_id,
            catalog,
            config,
            flows: Mutex::new(HashMap::new()),
            active_connections: Mutex::new(HashSet::new()),
            shutting_down: AtomicBool::new(false),
            shutdown: watch::channel(false).0,
            tasks: Arc::downgrade(&tasks),
            next_flow: AtomicU64::new(0),
            client,
            vault,
        });
        let (starts, receiver) = mpsc::channel(config.max_flows);
        let shutdown = inner.shutdown.subscribe();
        if !tasks.spawn_initial(run_start_worker(Arc::downgrade(&inner), receiver, shutdown)) {
            return Err(OAuthPublicError::new("oauth_task_owner_unavailable", true));
        }
        Ok(Self {
            inner,
            starts,
            tasks,
        })
    }

    pub(crate) fn availability(
        &self,
        provider: &str,
        vault_supported: bool,
    ) -> OAuthAvailabilityWire {
        self.inner.catalog.availability(provider, vault_supported)
    }

    pub(crate) fn try_start(
        &self,
        connection_id: &str,
        provider: String,
        desired_alias: String,
        attempt_id: String,
        route: OAuthRoute,
    ) -> Result<(), StartAdmissionError> {
        if self.inner.shutting_down.load(Ordering::Acquire) {
            return Err(StartAdmissionError::Closed);
        }
        self.inner
            .active_connections
            .lock()
            .map_err(|_| StartAdmissionError::Closed)?
            .insert(connection_id.to_owned());
        let job = StartJob {
            owner: FlowOwner {
                daemon_instance: self.inner.instance_id.clone(),
                connection_id: connection_id.to_owned(),
            },
            provider,
            desired_alias,
            attempt_id,
            route,
        };
        self.starts.try_send(job).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => StartAdmissionError::Busy,
            mpsc::error::TrySendError::Closed(_) => StartAdmissionError::Closed,
        })
    }

    pub(crate) fn status(
        &self,
        connection_id: &str,
        flow_id: &OAuthFlowId,
        attempt_id: &str,
    ) -> Option<OAuthFlowStatusWire> {
        let mut flows = self.inner.flows.lock().ok()?;
        let flow = flows.get_mut(flow_id.as_str())?;
        if !owns(flow, &self.inner.instance_id, connection_id, attempt_id) {
            return None;
        }
        expire_if_needed(flow);
        Some(public_status(flow))
    }

    pub(crate) fn cancel(
        &self,
        connection_id: &str,
        flow_id: &OAuthFlowId,
        attempt_id: &str,
    ) -> Option<OAuthFlowStatusWire> {
        let mut flows = self.inner.flows.lock().ok()?;
        let flow = flows.get_mut(flow_id.as_str())?;
        if !owns(flow, &self.inner.instance_id, connection_id, attempt_id) {
            return None;
        }
        expire_if_needed(flow);
        if !matches!(
            flow.status,
            InternalFlowStatus::Failed(_)
                | InternalFlowStatus::Expired
                | InternalFlowStatus::Cancelled
        ) {
            flow.cancel.send_replace(true);
            flow.status = InternalFlowStatus::Cancelled;
        }
        Some(public_status(flow))
    }

    pub(crate) fn cancel_connection(&self, connection_id: &str) {
        if let Ok(mut active) = self.inner.active_connections.lock() {
            active.remove(connection_id);
        }
        if let Ok(mut flows) = self.inner.flows.lock() {
            let owned = flows
                .iter()
                .filter(|(_, flow)| flow.owner.connection_id == connection_id)
                .map(|(flow_id, _)| flow_id.clone())
                .collect::<Vec<_>>();
            for flow_id in owned {
                if let Some(flow) = flows.remove(&flow_id) {
                    flow.cancel.send_replace(true);
                }
            }
        }
    }

    pub(crate) async fn shutdown(&self) -> bool {
        self.inner.shutting_down.store(true, Ordering::Release);
        self.inner.shutdown.send_replace(true);
        if let Ok(mut active) = self.inner.active_connections.lock() {
            active.clear();
        }
        if let Ok(mut flows) = self.inner.flows.lock() {
            for (_, flow) in flows.drain() {
                flow.cancel.send_replace(true);
            }
        }
        self.tasks.join_all().await
    }

    pub(crate) async fn abort_and_join(&self) {
        self.inner.shutting_down.store(true, Ordering::Release);
        self.inner.shutdown.send_replace(true);
        if let Ok(mut active) = self.inner.active_connections.lock() {
            active.clear();
        }
        if let Ok(mut flows) = self.inner.flows.lock() {
            for (_, flow) in flows.drain() {
                flow.cancel.send_replace(true);
            }
        }
        self.tasks.abort_and_join().await;
    }

    pub(crate) fn claim_ready(
        &self,
        connection_id: &str,
        flow_id: &OAuthFlowId,
        attempt_id: &str,
        provider: &str,
        alias: &str,
        reference: &OAuthReadyRefWire,
    ) -> Option<OAuthReadyClaim> {
        let mut flows = self.inner.flows.lock().ok()?;
        let mut flow = flows.remove(flow_id.as_str())?;
        expire_if_needed(&mut flow);
        if !owns(&flow, &self.inner.instance_id, connection_id, attempt_id)
            || flow.provider != provider
            || flow.desired_alias != alias
        {
            flows.insert(flow_id.as_str().to_owned(), flow);
            return None;
        }
        let InternalFlowStatus::Ready {
            reference: expected,
            bundle,
        } = flow.status
        else {
            flows.insert(flow_id.as_str().to_owned(), flow);
            return None;
        };
        if !constant_time_equal(expected.as_bytes(), reference.expose_reference().as_bytes()) {
            flow.status = InternalFlowStatus::Ready {
                reference: expected,
                bundle,
            };
            flows.insert(flow_id.as_str().to_owned(), flow);
            return None;
        }
        Some(OAuthReadyClaim {
            flow_id: flow_id.clone(),
            owner: flow.owner,
            provider: flow.provider,
            desired_alias: flow.desired_alias,
            attempt_id: flow.attempt_id,
            expires_at_ms: flow.expires_at_ms,
            deadline: flow.deadline,
            reference: expected,
            bundle: *bundle,
        })
    }

    pub(crate) fn restore_ready(&self, claim: OAuthReadyClaim) {
        if claim.owner.daemon_instance != self.inner.instance_id
            || now_ms().is_none_or(|now| now >= claim.expires_at_ms)
        {
            return;
        }
        let (cancel, _) = watch::channel(false);
        if let Ok(mut flows) = self.inner.flows.lock()
            && flows.len() < self.inner.config.max_flows
            && !flows.contains_key(claim.flow_id.as_str())
        {
            flows.insert(
                claim.flow_id.as_str().to_owned(),
                FlowEntry {
                    owner: claim.owner,
                    provider: claim.provider,
                    desired_alias: claim.desired_alias,
                    attempt_id: claim.attempt_id,
                    deadline: claim.deadline,
                    expires_at_ms: claim.expires_at_ms,
                    status: InternalFlowStatus::Ready {
                        reference: claim.reference,
                        bundle: Box::new(claim.bundle),
                    },
                    cancel,
                },
            );
        }
    }

    #[cfg(test)]
    fn flow_count(&self) -> usize {
        self.inner.flows.lock().map_or(0, |flows| flows.len())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StartAdmissionError {
    Busy,
    Closed,
}

pub(crate) struct OAuthReadyClaim {
    pub(crate) flow_id: OAuthFlowId,
    owner: FlowOwner,
    pub(crate) provider: String,
    pub(crate) desired_alias: String,
    pub(crate) attempt_id: String,
    expires_at_ms: u64,
    deadline: Instant,
    reference: Zeroizing<String>,
    pub(crate) bundle: OAuthTokenBundleV1,
}

#[cfg(test)]
impl OAuthReadyClaim {
    pub(crate) fn for_account_test(
        provider: &str,
        desired_alias: &str,
        bundle: OAuthTokenBundleV1,
    ) -> Self {
        Self {
            flow_id: OAuthFlowId::new("oauth-flow-account-test"),
            owner: FlowOwner {
                daemon_instance: "daemon-account-test".into(),
                connection_id: "connection-account-test".into(),
            },
            provider: provider.into(),
            desired_alias: desired_alias.into(),
            attempt_id: "attempt-account-test".into(),
            expires_at_ms: u64::MAX,
            deadline: Instant::now() + Duration::from_secs(60),
            reference: Zeroizing::new("oauth-ready-account-test".into()),
            bundle,
        }
    }
}

async fn run_start_worker(
    inner: Weak<CoordinatorInner>,
    mut receiver: mpsc::Receiver<StartJob>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        let job = tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return;
                }
                continue;
            }
            job = receiver.recv() => job,
        };
        let Some(job) = job else {
            return;
        };
        let Some(inner) = inner.upgrade() else {
            return;
        };
        begin_flow(inner, job).await;
    }
}

async fn begin_flow(inner: Arc<CoordinatorInner>, job: StartJob) {
    let owner_connection = job.owner.connection_id.clone();
    if !connection_is_active(&inner, &owner_connection) {
        return;
    }
    let availability = inner.catalog.availability(&job.provider, true);
    let Some(registration) = inner.catalog.registration(&job.provider) else {
        respond(
            &job.route,
            ResponseBody::AccountOAuthStart {
                availability,
                flow_id: None,
                authorization_url: None,
                provider_origin: None,
                loopback_port: None,
                expires_at_ms: None,
                user_code: None,
            },
        );
        return;
    };
    if !valid_alias(&job.desired_alias) || job.attempt_id.trim().is_empty() {
        respond_error(
            &job.route,
            "invalid_argument",
            "OAuth alias or attempt id is invalid",
            false,
        );
        return;
    }
    let at_capacity = inner.flows.lock().map_or(true, |mut flows| {
        !reserve_flow_capacity(&mut flows, inner.config.max_flows)
    });
    if at_capacity {
        respond_error(
            &job.route,
            ERROR_CODE_BUSY,
            "OAuth flow capacity is full; retry after another flow finishes",
            true,
        );
        return;
    }
    if registration.flow_mode == OAuthFlowMode::DeviceCode {
        begin_device_flow(inner, job, registration).await;
        return;
    }
    let listener = match TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await {
        Ok(listener) => listener,
        Err(_) => {
            respond_error(
                &job.route,
                "oauth_listener_unavailable",
                "cannot allocate a numeric loopback callback listener",
                true,
            );
            return;
        }
    };
    let Ok(SocketAddr::V4(bound)) = listener.local_addr() else {
        respond_error(
            &job.route,
            "oauth_listener_unavailable",
            "callback listener was not numeric IPv4 loopback",
            true,
        );
        return;
    };
    if !bound.ip().is_loopback() {
        respond_error(
            &job.route,
            "oauth_listener_unavailable",
            "callback listener was not loopback-only",
            false,
        );
        return;
    }
    if !connection_is_active(&inner, &owner_connection) {
        return;
    }
    let state = match random_secret(MIN_RANDOM_BYTES) {
        Ok(value) => value,
        Err(error) => {
            respond_public_error(&job.route, error);
            return;
        }
    };
    let verifier = match random_secret(MIN_RANDOM_BYTES) {
        Ok(value) => value,
        Err(error) => {
            respond_public_error(&job.route, error);
            return;
        }
    };
    let nonce = match random_secret(MIN_RANDOM_BYTES) {
        Ok(value) => value,
        Err(error) => {
            respond_public_error(&job.route, error);
            return;
        }
    };
    let callback_random = match random_secret(MIN_RANDOM_BYTES) {
        Ok(value) => value,
        Err(error) => {
            respond_public_error(&job.route, error);
            return;
        }
    };
    let verifier_b64 = Zeroizing::new(URL_SAFE_NO_PAD.encode(verifier.as_slice()));
    let state_b64 = Zeroizing::new(URL_SAFE_NO_PAD.encode(state.as_slice()));
    let nonce_b64 = Zeroizing::new(URL_SAFE_NO_PAD.encode(nonce.as_slice()));
    let callback_segment = Zeroizing::new(URL_SAFE_NO_PAD.encode(callback_random.as_slice()));
    let (path, uri, callback_authority) =
        compose_redirect(&job.provider, bound.port(), callback_segment.as_str());
    let callback_path = Zeroizing::new(path);
    let redirect_uri = Zeroizing::new(uri);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier_b64.as_bytes()));
    let authorization_url = {
        let mut authorization = url::form_urlencoded::Serializer::new(SecretFormBody::new(
            format!("{}?", registration.authorization_endpoint),
        ));
        authorization
            .append_pair("response_type", "code")
            .append_pair("client_id", &registration.client_id)
            .append_pair("redirect_uri", redirect_uri.as_str())
            .append_pair("scope", &registration.scopes.join(" "))
            .append_pair("state", state_b64.as_str())
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");
        if registration.send_nonce_in_authorize {
            authorization.append_pair("nonce", nonce_b64.as_str());
        }
        if registration.send_audience_in_authorize {
            authorization.append_pair("audience", &registration.audience);
        }
        if let Some(resource) = &registration.resource {
            authorization.append_pair("resource", resource);
        }
        for (name, value) in &registration.authorize_parameters {
            authorization.append_pair(name, value);
        }
        // `finish` moves the one state-bearing allocation directly into the
        // zeroizing wire wrapper. No second ordinary URL retains the query.
        OAuthAuthorizationWire::from_zeroizing(authorization.finish().into_zeroizing())
    };
    let flow_id = match random_id(
        "oauth-flow",
        inner.next_flow.fetch_add(1, Ordering::Relaxed),
    ) {
        Ok(flow_id) => OAuthFlowId::new(flow_id),
        Err(error) => {
            respond_public_error(&job.route, error);
            return;
        }
    };
    let (cancel, cancel_rx) = watch::channel(false);
    let expires_at_ms = now_ms()
        .unwrap_or(u64::MAX)
        .saturating_add(duration_ms(inner.config.flow_ttl));
    let deadline = Instant::now()
        .checked_add(inner.config.flow_ttl)
        .unwrap_or_else(Instant::now);
    let flow = FlowEntry {
        owner: job.owner,
        provider: job.provider,
        desired_alias: job.desired_alias,
        attempt_id: job.attempt_id,
        deadline,
        expires_at_ms,
        status: InternalFlowStatus::WaitingBrowser,
        cancel,
    };
    if let Ok(mut flows) = inner.flows.lock() {
        if !reserve_flow_capacity(&mut flows, inner.config.max_flows) {
            respond_error(
                &job.route,
                ERROR_CODE_BUSY,
                "OAuth flow capacity is full; retry after another flow finishes",
                true,
            );
            return;
        }
        flows.insert(flow_id.as_str().to_owned(), flow);
    } else {
        respond_error(
            &job.route,
            "oauth_internal",
            "OAuth coordinator is unavailable",
            true,
        );
        return;
    }
    // Close the enqueue/disconnect race: if cleanup ran before insertion,
    // remove the just-created flow before exposing its URL or listener.
    if !connection_is_active(&inner, &owner_connection) {
        if let Ok(mut flows) = inner.flows.lock()
            && let Some(flow) = flows.remove(flow_id.as_str())
        {
            flow.cancel.send_replace(true);
        }
        return;
    }
    respond(
        &job.route,
        ResponseBody::AccountOAuthStart {
            availability,
            flow_id: Some(flow_id.clone()),
            authorization_url: Some(authorization_url),
            provider_origin: Some(safe_url(&registration.authorization_endpoint)),
            loopback_port: Some(bound.port()),
            expires_at_ms: Some(expires_at_ms),
            // Loopback/PKCE has no user code: the browser callback carries
            // the grant, there is nothing for a human to type.
            user_code: None,
        },
    );
    let task_inner = Arc::clone(&inner);
    let Some(tasks) = inner.tasks.upgrade() else {
        return;
    };
    if !tasks
        .spawn(run_callback_flow(
            task_inner,
            flow_id.clone(),
            registration,
            listener,
            callback_path,
            callback_authority,
            state_b64,
            verifier_b64,
            nonce_b64,
            redirect_uri,
            cancel_rx,
        ))
        .await
        && let Ok(mut flows) = inner.flows.lock()
        && let Some(flow) = flows.remove(flow_id.as_str())
    {
        flow.cancel.send_replace(true);
    }
}

#[derive(Deserialize)]
struct DeviceAuthorizationResponse {
    device_code: SecretJson,
    user_code: SecretJson,
    #[serde(default)]
    verification_uri_complete: Option<SecretJson>,
    #[serde(default)]
    verification_uri: Option<SecretJson>,
    expires_in: u64,
    #[serde(default = "default_device_poll_interval")]
    interval: u64,
}

const fn default_device_poll_interval() -> u64 {
    5
}

enum DeviceTokenPoll {
    Tokens(Zeroizing<Vec<u8>>),
    Pending,
    SlowDown,
    Expired,
    Denied,
    Retryable,
}

async fn begin_device_flow(
    inner: Arc<CoordinatorInner>,
    job: StartJob,
    registration: Arc<OAuthProviderRegistration>,
) {
    let owner_connection = job.owner.connection_id.clone();
    let device_id = if registration.auth_header_set == OAuthAuthHeaderSet::KimiMsh {
        match load_or_create_kimi_device_id(Arc::clone(&inner.vault)).await {
            Ok(device_id) => Some(device_id),
            Err(error) => {
                respond_public_error(&job.route, error);
                return;
            }
        }
    } else {
        None
    };
    let authorization = match request_device_authorization(
        &inner.client,
        &registration,
        device_id.as_ref(),
    )
    .await
    {
        Ok(authorization) => authorization,
        Err(error) => {
            respond_public_error(&job.route, error);
            return;
        }
    };
    if authorization.expires_in == 0
        || authorization.expires_in > MAX_TOKEN_LIFETIME_SECS
        || authorization.interval == 0
        || authorization.user_code.0.is_empty()
        || authorization.device_code.0.is_empty()
    {
        respond_public_error(
            &job.route,
            OAuthPublicError::new("invalid_device_authorization_response", false),
        );
        return;
    }
    let verification = authorization
        .verification_uri_complete
        .as_ref()
        .and_then(|value| std::str::from_utf8(&value.0).ok())
        .and_then(|value| Url::parse(value).ok())
        .or_else(|| {
            let mut url = authorization
                .verification_uri
                .as_ref()
                .and_then(|value| std::str::from_utf8(&value.0).ok())
                .and_then(|value| Url::parse(value).ok())?;
            let user_code = std::str::from_utf8(&authorization.user_code.0).ok()?;
            url.query_pairs_mut().append_pair("user_code", user_code);
            Some(url)
        })
        .filter(|url| same_origin(url, &registration.authorization_endpoint));
    let verification = match verification {
        Some(verification) => verification,
        None => {
            respond_public_error(
                &job.route,
                OAuthPublicError::new("invalid_device_authorization_response", false),
            );
            return;
        }
    };
    // Publish the user code alongside the URL (v0.0.938). It comes from the
    // same authorization response the URL is built from, so the two can never
    // disagree; a client that shows the code no longer parses it back out of
    // the URL's query string.
    let device_user_code = std::str::from_utf8(&authorization.user_code.0)
        .ok()
        .map(str::to_owned);
    let authorization_url =
        OAuthAuthorizationWire::from_zeroizing(Zeroizing::new(verification.as_str().to_owned()));
    let flow_id = match random_id(
        "oauth-flow",
        inner.next_flow.fetch_add(1, Ordering::Relaxed),
    ) {
        Ok(flow_id) => OAuthFlowId::new(flow_id),
        Err(error) => {
            respond_public_error(&job.route, error);
            return;
        }
    };
    let server_ttl = Duration::from_secs(authorization.expires_in);
    let flow_ttl = inner.config.flow_ttl.min(server_ttl);
    let expires_at_ms = now_ms()
        .unwrap_or(u64::MAX)
        .saturating_add(duration_ms(flow_ttl));
    let deadline = Instant::now()
        .checked_add(flow_ttl)
        .unwrap_or_else(Instant::now);
    let (cancel, cancel_rx) = watch::channel(false);
    let flow = FlowEntry {
        owner: job.owner,
        provider: job.provider,
        desired_alias: job.desired_alias,
        attempt_id: job.attempt_id,
        deadline,
        expires_at_ms,
        status: InternalFlowStatus::WaitingDevice,
        cancel,
    };
    if let Ok(mut flows) = inner.flows.lock() {
        if !reserve_flow_capacity(&mut flows, inner.config.max_flows) {
            respond_error(
                &job.route,
                ERROR_CODE_BUSY,
                "OAuth flow capacity is full; retry after another flow finishes",
                true,
            );
            return;
        }
        flows.insert(flow_id.as_str().to_owned(), flow);
    } else {
        respond_error(
            &job.route,
            "oauth_internal",
            "OAuth coordinator is unavailable",
            true,
        );
        return;
    }
    // Close the enqueue/disconnect race exactly as the loopback flow does.
    // Cleanup may have run while device authorization was in flight.
    if !connection_is_active(&inner, &owner_connection) {
        if let Ok(mut flows) = inner.flows.lock()
            && let Some(flow) = flows.remove(flow_id.as_str())
        {
            flow.cancel.send_replace(true);
        }
        return;
    }
    respond(
        &job.route,
        ResponseBody::AccountOAuthStart {
            availability: inner.catalog.availability(&registration.provider_id, true),
            flow_id: Some(flow_id.clone()),
            authorization_url: Some(authorization_url),
            provider_origin: Some(safe_url(&registration.authorization_endpoint)),
            loopback_port: None,
            expires_at_ms: Some(expires_at_ms),
            user_code: device_user_code,
        },
    );
    let Some(tasks) = inner.tasks.upgrade() else {
        set_terminal(
            &inner,
            &flow_id,
            InternalFlowStatus::Failed("oauth_task_owner_unavailable"),
        );
        return;
    };
    let task_inner = Arc::clone(&inner);
    if !tasks
        .spawn(run_device_token_flow(
            task_inner,
            flow_id.clone(),
            registration,
            authorization.device_code.0,
            device_id,
            Duration::from_secs(authorization.interval),
            flow_ttl,
            cancel_rx,
        ))
        .await
    {
        set_terminal(
            &inner,
            &flow_id,
            InternalFlowStatus::Failed("oauth_task_owner_unavailable"),
        );
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_device_token_flow(
    inner: Arc<CoordinatorInner>,
    flow_id: OAuthFlowId,
    registration: Arc<OAuthProviderRegistration>,
    device_code: Zeroizing<Vec<u8>>,
    device_id: Option<SecretHandle>,
    mut interval: Duration,
    flow_ttl: Duration,
    mut cancel: watch::Receiver<bool>,
) {
    let deadline = tokio::time::sleep(flow_ttl);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => {
                set_terminal(&inner, &flow_id, InternalFlowStatus::Expired);
                return;
            }
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    set_terminal(&inner, &flow_id, InternalFlowStatus::Cancelled);
                    return;
                }
            }
            () = tokio::time::sleep(interval) => {}
        }
        let poll = poll_device_token(
            &inner.client,
            &registration,
            &device_code,
            device_id.as_ref(),
        );
        tokio::pin!(poll);
        let result = tokio::select! {
            _ = &mut deadline => {
                set_terminal(&inner, &flow_id, InternalFlowStatus::Expired);
                return;
            }
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    set_terminal(&inner, &flow_id, InternalFlowStatus::Cancelled);
                    return;
                }
                continue;
            }
            result = &mut poll => result,
        };
        match result {
            Ok(DeviceTokenPoll::Pending | DeviceTokenPoll::Retryable) => {}
            Ok(DeviceTokenPoll::SlowDown) => {
                interval = interval.saturating_add(KIMI_DEVICE_POLL_SLOW_DOWN);
            }
            Ok(DeviceTokenPoll::Expired) => {
                set_terminal(&inner, &flow_id, InternalFlowStatus::Expired);
                return;
            }
            Ok(DeviceTokenPoll::Denied) => {
                set_terminal(
                    &inner,
                    &flow_id,
                    InternalFlowStatus::Failed("access_denied"),
                );
                return;
            }
            Ok(DeviceTokenPoll::Tokens(bytes)) => {
                set_terminal(&inner, &flow_id, InternalFlowStatus::Exchanging);
                match token_bundle_from_response(&registration, &bytes, &[], 1, None).await {
                    Ok(bundle) => {
                        if registration.provider_id == haider_provider::KIMI_OAUTH_PROVIDER_NAME
                            && let Err(error) = validate_kimi_oauth_bundle(&bundle).await
                        {
                            set_terminal(&inner, &flow_id, InternalFlowStatus::Failed(error.code));
                            return;
                        }
                        let reference = match random_id("oauth-ready", 0) {
                            Ok(reference) => Zeroizing::new(reference),
                            Err(error) => {
                                set_terminal(
                                    &inner,
                                    &flow_id,
                                    InternalFlowStatus::Failed(error.code),
                                );
                                return;
                            }
                        };
                        set_terminal(
                            &inner,
                            &flow_id,
                            InternalFlowStatus::Ready {
                                reference,
                                bundle: Box::new(bundle),
                            },
                        );
                    }
                    Err(error) => {
                        set_terminal(&inner, &flow_id, InternalFlowStatus::Failed(error.code))
                    }
                }
                return;
            }
            Err(error) => {
                set_terminal(&inner, &flow_id, InternalFlowStatus::Failed(error.code));
                return;
            }
        }
    }
}

async fn validate_kimi_oauth_bundle(bundle: &OAuthTokenBundleV1) -> Result<(), OAuthPublicError> {
    let adapter = haider_provider::OpenAiCompatibleProvider::new_kimi_subscription(
        bundle.access_token_handle(),
        "kimi-for-coding",
        haider_provider::KIMI_OAUTH_BASE_URL,
    )
    .map_err(|_| OAuthPublicError::new("inference_validation_unavailable", true))?;
    let request = haider_provider::TurnRequest {
        messages: vec![haider_provider::Message::user_text("ping")],
        model: "kimi-for-coding".to_owned(),
        max_tokens: 1,
        system_prompt: None,
        tools: Vec::new(),
        attachments: Vec::new(),
        cache_metadata: None,
    };
    let mut stream = adapter
        .stream_turn(request)
        .await
        .map_err(|error| match error.kind {
            haider_provider::ProviderErrorKind::Authentication => {
                OAuthPublicError::new("inference_validation_unauthorized", false)
            }
            haider_provider::ProviderErrorKind::PermissionDenied => {
                OAuthPublicError::new("inference_validation_forbidden", false)
            }
            _ => OAuthPublicError::new("inference_validation_unavailable", true),
        })?;
    match stream.recv().await {
        Some(Err(error)) => Err(match error.kind {
            haider_provider::ProviderErrorKind::Authentication => {
                OAuthPublicError::new("inference_validation_unauthorized", false)
            }
            haider_provider::ProviderErrorKind::PermissionDenied => {
                OAuthPublicError::new("inference_validation_forbidden", false)
            }
            _ => OAuthPublicError::new("inference_validation_unavailable", true),
        }),
        Some(Ok(_)) | None => Ok(()),
    }
}

async fn request_device_authorization(
    client: &reqwest::Client,
    registration: &OAuthProviderRegistration,
    device_id: Option<&SecretHandle>,
) -> Result<DeviceAuthorizationResponse, OAuthPublicError> {
    let body = {
        let mut encoded = url::form_urlencoded::Serializer::new(SecretFormBody::empty());
        encoded.append_pair("client_id", &registration.client_id);
        if !registration.scopes.is_empty() {
            encoded.append_pair("scope", &registration.scopes.join(" "));
        }
        encoded.finish()
    };
    let request = client
        .post(registration.authorization_endpoint.clone())
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .header(reqwest::header::CONNECTION, "close")
        .body(reqwest::Body::from(bytes::Bytes::from_owner(body)));
    let response = apply_oauth_auth_headers(request, registration, device_id)?
        .send()
        .await
        .map_err(|_| OAuthPublicError::new("device_authorization_unavailable", true))?;
    if response.status().is_redirection() {
        return Err(OAuthPublicError::new("token_redirect_rejected", false));
    }
    let status = response.status();
    let bytes = bounded_response(response).await?;
    if !status.is_success() {
        return Err(OAuthPublicError::new(
            "device_authorization_failed",
            status.as_u16() == 429 || status.is_server_error(),
        ));
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| OAuthPublicError::new("invalid_device_authorization_response", false))
}

async fn poll_device_token(
    client: &reqwest::Client,
    registration: &OAuthProviderRegistration,
    device_code: &[u8],
    device_id: Option<&SecretHandle>,
) -> Result<DeviceTokenPoll, OAuthPublicError> {
    let device_code = std::str::from_utf8(device_code)
        .map_err(|_| OAuthPublicError::new("invalid_device_code", false))?;
    let body = {
        let mut encoded = url::form_urlencoded::Serializer::new(SecretFormBody::empty());
        encoded
            .append_pair("grant_type", "urn:ietf:params:oauth:grant-type:device_code")
            .append_pair("device_code", device_code)
            .append_pair("client_id", &registration.client_id);
        encoded.finish()
    };
    let request = client
        .post(registration.token_endpoint.clone())
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .header(reqwest::header::CONNECTION, "close")
        .body(reqwest::Body::from(bytes::Bytes::from_owner(body)));
    let response = apply_oauth_auth_headers(request, registration, device_id)?
        .send()
        .await
        .map_err(|_| OAuthPublicError::new("token_endpoint_unavailable", true))?;
    if response.status().is_redirection() {
        return Err(OAuthPublicError::new("token_redirect_rejected", false));
    }
    let status = response.status();
    let bytes = bounded_response(response).await?;
    if status.is_success() {
        return Ok(DeviceTokenPoll::Tokens(bytes));
    }
    #[derive(Deserialize)]
    struct DeviceErrorBody {
        #[serde(default)]
        error: Option<SecretJson>,
    }
    let error = serde_json::from_slice::<DeviceErrorBody>(&bytes)
        .ok()
        .and_then(|body| body.error);
    let is = |name: &[u8]| {
        error
            .as_ref()
            .is_some_and(|error| error.0.as_slice() == name)
    };
    if is(b"authorization_pending") {
        Ok(DeviceTokenPoll::Pending)
    } else if is(b"slow_down") {
        Ok(DeviceTokenPoll::SlowDown)
    } else if is(b"expired_token") {
        Ok(DeviceTokenPoll::Expired)
    } else if is(b"access_denied") {
        Ok(DeviceTokenPoll::Denied)
    } else if status.as_u16() == 429 || status.is_server_error() {
        Ok(DeviceTokenPoll::Retryable)
    } else {
        Err(OAuthPublicError::new("token_exchange_failed", false))
    }
}

fn apply_oauth_auth_headers(
    mut request: reqwest::RequestBuilder,
    registration: &OAuthProviderRegistration,
    device_id: Option<&SecretHandle>,
) -> Result<reqwest::RequestBuilder, OAuthPublicError> {
    if registration.auth_header_set == OAuthAuthHeaderSet::Standard {
        return Ok(request);
    }
    let device_id =
        device_id.ok_or_else(|| OAuthPublicError::new("device_identity_unavailable", true))?;
    let mut value = reqwest::header::HeaderValue::from_bytes(device_id.expose_secret())
        .map_err(|_| OAuthPublicError::new("device_identity_invalid", false))?;
    value.set_sensitive(true);
    request = request
        .header("X-Msh-Platform", "kimi_cli")
        .header("X-Msh-Version", env!("CARGO_PKG_VERSION"))
        .header("X-Msh-Device-Name", "haider-agent")
        .header("X-Msh-Device-Model", std::env::consts::ARCH)
        .header("X-Msh-Os-Version", std::env::consts::OS)
        .header("X-Msh-Device-Id", value);
    Ok(request)
}

async fn load_or_create_kimi_device_id(
    vault: Arc<dyn Vault>,
) -> Result<SecretHandle, OAuthPublicError> {
    let alias = CredentialAlias::new(KIMI_DEVICE_ALIAS);
    let lease = tokio::time::timeout(
        TOKEN_TIMEOUT,
        acquire_refresh_lock(Arc::clone(&vault), &alias),
    )
    .await
    .map_err(|_| OAuthPublicError::new("device_identity_unavailable", true))?
    .map_err(|_| OAuthPublicError::new("device_identity_unavailable", true))?;
    let vault_for_read = Arc::clone(&vault);
    let alias_for_read = alias.clone();
    let resolved = tokio::task::spawn_blocking(move || vault_for_read.resolve(&alias_for_read))
        .await
        .map_err(|_| OAuthPublicError::new("device_identity_unavailable", true))?;
    match resolved {
        Ok(device_id)
            if crate::device_discovery::valid_kimi_device_id(device_id.expose_secret()) =>
        {
            drop(lease);
            return Ok(device_id);
        }
        Ok(_) => return Err(OAuthPublicError::new("device_identity_invalid", false)),
        Err(error) if error.code == ErrorCode::CredentialMissing => {}
        Err(_) => return Err(OAuthPublicError::new("device_identity_unavailable", true)),
    }
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random)
        .map_err(|_| OAuthPublicError::new("randomness_unavailable", true))?;
    random[6] = (random[6] & 0x0f) | 0x40;
    random[8] = (random[8] & 0x3f) | 0x80;
    let device_id = Zeroizing::new(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        random[0],
        random[1],
        random[2],
        random[3],
        random[4],
        random[5],
        random[6],
        random[7],
        random[8],
        random[9],
        random[10],
        random[11],
        random[12],
        random[13],
        random[14],
        random[15]
    ));
    random.zeroize();
    let vault_for_put = Arc::clone(&vault);
    let alias_for_put = alias.clone();
    let bytes_for_put = Zeroizing::new(device_id.as_bytes().to_vec());
    tokio::task::spawn_blocking(move || vault_for_put.put(&alias_for_put, &bytes_for_put))
        .await
        .map_err(|_| OAuthPublicError::new("device_identity_unavailable", true))?
        .map_err(|_| OAuthPublicError::new("device_identity_unavailable", true))?;
    drop(lease);
    tokio::task::spawn_blocking(move || vault.resolve(&alias))
        .await
        .map_err(|_| OAuthPublicError::new("device_identity_unavailable", true))?
        .map_err(|_| OAuthPublicError::new("device_identity_unavailable", true))
}

fn same_origin(left: &Url, right: &Url) -> bool {
    left.scheme() == right.scheme()
        && left.host_str() == right.host_str()
        && left.port_or_known_default() == right.port_or_known_default()
        && left.username().is_empty()
        && left.password().is_none()
        && left.fragment().is_none()
}

#[allow(clippy::too_many_arguments)]
async fn run_callback_flow(
    inner: Arc<CoordinatorInner>,
    flow_id: OAuthFlowId,
    registration: Arc<OAuthProviderRegistration>,
    listener: TcpListener,
    callback_path: Zeroizing<String>,
    callback_authority: String,
    expected_state: Zeroizing<String>,
    verifier: Zeroizing<String>,
    nonce: Zeroizing<String>,
    redirect_uri: Zeroizing<String>,
    mut cancel: watch::Receiver<bool>,
) {
    enum CallbackWait {
        Expired,
        Cancelled,
        Complete(CallbackResult),
    }

    let deadline = tokio::time::sleep(inner.config.flow_ttl);
    tokio::pin!(deadline);
    let mut invalid = 0_usize;
    loop {
        tokio::select! {
            _ = &mut deadline => {
                set_terminal(&inner, &flow_id, InternalFlowStatus::Expired);
                return;
            }
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    set_terminal(&inner, &flow_id, InternalFlowStatus::Cancelled);
                    return;
                }
            }
            accepted = listener.accept() => {
                let Ok((mut stream, peer)) = accepted else {
                    set_terminal(&inner, &flow_id, InternalFlowStatus::Failed("callback_listener_failed"));
                    return;
                };
                if !peer.ip().is_loopback() {
                    invalid = invalid.saturating_add(1);
                    continue;
                }
                let callback_wait = {
                    let callback = read_callback(
                        &mut stream,
                        callback_path.as_str(),
                        callback_authority.as_str(),
                        expected_state.as_bytes(),
                    );
                    tokio::pin!(callback);
                    tokio::select! {
                        _ = &mut deadline => CallbackWait::Expired,
                        changed = cancel.changed() => {
                            if changed.is_err() || *cancel.borrow() {
                                CallbackWait::Cancelled
                            } else {
                                continue;
                            }
                        }
                        result = &mut callback => CallbackWait::Complete(result),
                    }
                };
                let callback_result = match callback_wait {
                    CallbackWait::Expired => {
                        // An explicit half-close wakes Windows peers promptly;
                        // relying on handle drop alone can defer observable EOF.
                        let _ = stream.shutdown().await;
                        set_terminal(&inner, &flow_id, InternalFlowStatus::Expired);
                        return;
                    }
                    CallbackWait::Cancelled => {
                        let _ = stream.shutdown().await;
                        set_terminal(&inner, &flow_id, InternalFlowStatus::Cancelled);
                        return;
                    }
                    CallbackWait::Complete(result) => result,
                };
                match callback_result {
                    CallbackResult::Invalid(reason) => {
                        invalid = invalid.saturating_add(1);
                        let _ =
                            send_callback_page(&mut stream, 400, &rejection_html(reason)).await;
                        if invalid >= inner.config.max_invalid_callbacks {
                            set_terminal(
                                &inner,
                                &flow_id,
                                InternalFlowStatus::Failed("callback_interference"),
                            );
                            return;
                        }
                    }
                    CallbackResult::Denied(code) => {
                        let _ = send_callback_page(&mut stream, 200, DENIED_HTML).await;
                        set_terminal(&inner, &flow_id, InternalFlowStatus::Failed(code));
                        return;
                    }
                    CallbackResult::Code(code) => {
                        let _ = send_callback_page(&mut stream, 200, SUCCESS_HTML).await;
                        set_terminal(&inner, &flow_id, InternalFlowStatus::Exchanging);
                        let exchange = exchange_authorization_code(
                            &inner.client,
                            &registration,
                            code.as_slice(),
                            expected_state.as_bytes(),
                            verifier.as_bytes(),
                            nonce.as_bytes(),
                            redirect_uri.as_str(),
                        );
                        tokio::pin!(exchange);
                        tokio::select! {
                            _ = &mut deadline => {
                                set_terminal(&inner, &flow_id, InternalFlowStatus::Expired);
                            }
                            changed = cancel.changed() => {
                                if changed.is_err() || *cancel.borrow() {
                                    set_terminal(
                                        &inner,
                                        &flow_id,
                                        InternalFlowStatus::Cancelled,
                                    );
                                }
                            }
                            result = &mut exchange => match result {
                                Ok(bundle) => {
                                    let reference = match random_id("oauth-ready", 0) {
                                        Ok(value) => Zeroizing::new(value),
                                        Err(error) => {
                                            set_terminal(
                                                &inner,
                                                &flow_id,
                                                InternalFlowStatus::Failed(error.code),
                                            );
                                            return;
                                        }
                                    };
                                    set_terminal(
                                        &inner,
                                        &flow_id,
                                        InternalFlowStatus::Ready {
                                            reference,
                                            bundle: Box::new(bundle),
                                        },
                                    );
                                }
                                Err(error) => set_terminal(
                                    &inner,
                                    &flow_id,
                                    InternalFlowStatus::Failed(error.code),
                                ),
                            },
                        }
                        return;
                    }
                }
            }
        }
    }
}

/// WHY one loopback callback was rejected. Static copy only: the served
/// page must explain itself and how to retry, but must never echo request
/// data back to the browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallbackRejection {
    /// The bytes never parsed as a plain `GET` browser callback.
    MalformedRequest,
    /// Wrong `Host` authority or callback path — usually a stale link from
    /// an earlier sign-in attempt landing on a reused or foreign listener.
    WrongAddress,
    /// `state`/`code`/`error` did not match this flow — stale, replayed,
    /// duplicated, or from another sign-in attempt.
    WrongAttempt,
    /// Correct state, but the provider reported an error outside the RFC
    /// 6749 vocabulary this daemon recognizes.
    UnrecognizedProviderError,
}

impl CallbackRejection {
    const fn why(self) -> &'static str {
        match self {
            Self::MalformedRequest => {
                "the request did not parse as a plain browser sign-in callback"
            }
            Self::WrongAddress => {
                "it arrived on an address or path that does not belong to the sign-in attempt this listener is waiting for (often a leftover link from an earlier attempt)"
            }
            Self::WrongAttempt => {
                "its sign-in state does not match the attempt in progress, so it is stale, replayed, or from a different sign-in"
            }
            Self::UnrecognizedProviderError => {
                "the provider reported an error Haider does not recognize"
            }
        }
    }
}

/// Shared inline stylesheet for every loopback callback page. Fully
/// self-contained by law: no external stylesheet, font, image, or script may
/// ever be referenced — the page must render complete from these bytes alone
/// (`callback_pages_are_branded_and_fully_self_contained`).
macro_rules! callback_page_style {
    () => {
        "<style>\
:root{color-scheme:dark}\
body{margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;\
background:#0c0c10;color:#e8e4da;font:16px/1.65 Georgia,'Times New Roman',serif}\
main{max-width:27rem;padding:3rem 2.25rem;text-align:center}\
.wordmark{margin:0 0 1.5rem;font:600 .8rem/1 'Avenir Next','Helvetica Neue',Arial,sans-serif;\
letter-spacing:.5em;text-indent:.5em;text-transform:uppercase;color:#c9a35c}\
.rule{width:2.5rem;margin:0 auto 1.5rem;border:0;border-top:1px solid rgba(201,163,92,.4)}\
h1{margin:0 0 .8rem;font-size:1.3rem;font-weight:500;letter-spacing:.01em;color:#f3efe5}\
p{margin:0 0 .8rem;color:#a8a294}\
.hint{margin:1.4rem 0 0;font-size:.85rem;color:#6f6a5f}\
</style>"
    };
}

/// Opening boilerplate shared by every callback page: self-contained,
/// referrer-suppressed, mobile-sane. The `<title>` follows per page.
macro_rules! callback_page_head {
    () => {
        "<!doctype html><html lang=en><meta charset=utf-8>\
<meta name=viewport content=\"width=device-width,initial-scale=1\">\
<meta name=referrer content=no-referrer>"
    };
}

/// The Haider wordmark as TEXT plus its gold rule — never an image or font
/// request.
macro_rules! callback_page_wordmark {
    () => {
        "<body><main><p class=wordmark>Haider</p><hr class=rule>"
    };
}

/// The rejection page. Never a bare "rejected": it states WHY (static,
/// request-independent copy) and how to retry, quoting the flow TTL. The
/// only non-literal byte source is [`CallbackRejection::why`]'s fixed
/// four-reason vocabulary — request data NEVER reaches this format string.
fn rejection_html(reason: CallbackRejection) -> String {
    [
        concat!(
            callback_page_head!(),
            "<title>Haider — sign-in callback rejected</title>",
            callback_page_style!(),
            callback_page_wordmark!(),
            "<h1>Sign-in callback rejected</h1>",
            "<p>This callback was rejected: "
        ),
        reason.why(),
        concat!(
            ".</p>",
            "<p class=hint>To retry, return to Haider and start the sign-in again — each \
             sign-in link is valid for one attempt within 10 minutes.</p>",
            "</main></html>"
        ),
    ]
    .concat()
}

enum CallbackResult {
    Invalid(CallbackRejection),
    Denied(&'static str),
    Code(Zeroizing<Vec<u8>>),
}

async fn read_callback(
    stream: &mut TcpStream,
    expected_path: &str,
    expected_authority: &str,
    expected_state: &[u8],
) -> CallbackResult {
    let mut request = Zeroizing::new(Vec::with_capacity(1024));
    let read = async {
        let mut chunk = Zeroizing::new([0_u8; 1024]);
        loop {
            let count = stream.read(&mut chunk[..]).await.map_err(|_| ())?;
            if count == 0 {
                return Err(());
            }
            request.extend_from_slice(&chunk[..count]);
            if request.len() > CALLBACK_RESPONSE_LIMIT {
                return Err(());
            }
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                return Ok(());
            }
        }
    };
    if !matches!(
        tokio::time::timeout(CALLBACK_READ_TIMEOUT, read).await,
        Ok(Ok(()))
    ) {
        return CallbackResult::Invalid(CallbackRejection::MalformedRequest);
    }
    parse_callback(&request, expected_path, expected_authority, expected_state)
}

fn parse_callback(
    request: &[u8],
    expected_path: &str,
    expected_authority: &str,
    expected_state: &[u8],
) -> CallbackResult {
    let Ok(text) = std::str::from_utf8(request) else {
        return CallbackResult::Invalid(CallbackRejection::MalformedRequest);
    };
    if !text.is_ascii() {
        return CallbackResult::Invalid(CallbackRejection::MalformedRequest);
    }
    let Some(header_end) = text.find("\r\n\r\n") else {
        return CallbackResult::Invalid(CallbackRejection::MalformedRequest);
    };
    if !text[header_end + 4..].is_empty() {
        return CallbackResult::Invalid(CallbackRejection::MalformedRequest);
    }
    let mut lines = text[..header_end].split("\r\n");
    let Some(request_line) = lines.next() else {
        return CallbackResult::Invalid(CallbackRejection::MalformedRequest);
    };
    let mut parts = request_line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return CallbackResult::Invalid(CallbackRejection::MalformedRequest);
    };
    if method != "GET" || version != "HTTP/1.1" || !target.starts_with('/') || target.contains('#')
    {
        return CallbackResult::Invalid(CallbackRejection::MalformedRequest);
    }
    let mut host = None;
    let mut content_length = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            return CallbackResult::Invalid(CallbackRejection::MalformedRequest);
        };
        if name.eq_ignore_ascii_case("host") {
            if host.replace(value.trim()).is_some() {
                return CallbackResult::Invalid(CallbackRejection::MalformedRequest);
            }
        } else if name.eq_ignore_ascii_case("content-length") {
            if content_length.replace(value.trim()).is_some() {
                return CallbackResult::Invalid(CallbackRejection::MalformedRequest);
            }
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            return CallbackResult::Invalid(CallbackRejection::MalformedRequest);
        }
    }
    if content_length.is_some_and(|length| length != "0") {
        return CallbackResult::Invalid(CallbackRejection::MalformedRequest);
    }
    // The one authority law: the browser sends the exact `host:port` it
    // navigated to, which is the redirect authority THIS flow registered
    // with the provider (`compose_redirect` — `localhost:<port>` for
    // Anthropic parity, numeric `127.0.0.1:<port>` for everyone else).
    // Validating a recomputed shape instead of the flow's own composed
    // authority is the v0.0.65 owner bug.
    if host != Some(expected_authority) {
        return CallbackResult::Invalid(CallbackRejection::WrongAddress);
    }
    let Some((path, query)) = target.split_once('?') else {
        return CallbackResult::Invalid(CallbackRejection::WrongAddress);
    };
    if path != expected_path || query.is_empty() {
        return CallbackResult::Invalid(CallbackRejection::WrongAddress);
    }
    let mut states = Vec::new();
    let mut codes = Vec::new();
    let mut errors = Vec::new();
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        let value = Zeroizing::new(value.into_owned().into_bytes());
        match key.as_ref() {
            "state" => states.push(value),
            "code" => codes.push(value),
            "error" => errors.push(value),
            _ => {}
        }
    }
    if states.len() != 1
        || !constant_time_equal(expected_state, states[0].as_slice())
        || codes.len().saturating_add(errors.len()) != 1
        || codes.len() > 1
        || errors.len() > 1
    {
        return CallbackResult::Invalid(CallbackRejection::WrongAttempt);
    }
    if let Some(code) = codes.pop() {
        if code.is_empty() || code.len() > 4096 {
            return CallbackResult::Invalid(CallbackRejection::WrongAttempt);
        }
        CallbackResult::Code(code)
    } else {
        match errors.pop().as_ref().map(|error| error.as_slice()) {
            Some(b"access_denied") => CallbackResult::Denied("access_denied"),
            Some(
                b"invalid_request"
                | b"unauthorized_client"
                | b"unsupported_response_type"
                | b"invalid_scope"
                | b"server_error"
                | b"temporarily_unavailable",
            ) => CallbackResult::Denied("authorization_denied"),
            Some(_) => CallbackResult::Invalid(CallbackRejection::UnrecognizedProviderError),
            None => CallbackResult::Invalid(CallbackRejection::WrongAttempt),
        }
    }
}

fn constant_time_equal(expected: &[u8], supplied: &[u8]) -> bool {
    expected.len() == supplied.len() && bool::from(expected.ct_eq(supplied))
}

struct SecretFormBody(Zeroizing<String>);

impl SecretFormBody {
    fn empty() -> Self {
        Self(Zeroizing::new(String::new()))
    }

    fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }

    fn into_zeroizing(mut self) -> Zeroizing<String> {
        Zeroizing::new(std::mem::take(&mut *self.0))
    }
}

impl url::form_urlencoded::Target for SecretFormBody {
    type Finished = Self;

    fn as_mut_string(&mut self) -> &mut String {
        &mut self.0
    }

    fn finish(self) -> Self::Finished {
        self
    }
}

impl AsRef<[u8]> for SecretFormBody {
    fn as_ref(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

enum SecretTokenBody {
    Form(SecretFormBody),
    Json(Zeroizing<Vec<u8>>),
}

impl SecretTokenBody {
    fn content_type(&self) -> &'static str {
        match self {
            Self::Form(_) => "application/x-www-form-urlencoded",
            Self::Json(_) => "application/json",
        }
    }
}

impl AsRef<[u8]> for SecretTokenBody {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Form(body) => body.as_ref(),
            Self::Json(body) => body.as_slice(),
        }
    }
}

#[derive(Serialize)]
struct AuthorizationCodeRequest<'a> {
    grant_type: &'static str,
    code: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<&'a str>,
    client_id: &'a str,
    redirect_uri: &'a str,
    code_verifier: &'a str,
}

#[derive(Serialize)]
struct RefreshTokenRequest<'a> {
    grant_type: &'static str,
    refresh_token: &'a str,
    client_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    audience: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resource: Option<&'a str>,
}

fn encode_secret_json<T: Serialize>(value: &T) -> Result<Zeroizing<Vec<u8>>, OAuthPublicError> {
    let mut body = Zeroizing::new(Vec::new());
    serde_json::to_writer(&mut *body, value)
        .map_err(|_| OAuthPublicError::new("oauth_request_encoding_failed", false))?;
    Ok(body)
}

fn authorization_code_request_body(
    registration: &OAuthProviderRegistration,
    code: &[u8],
    state: &[u8],
    verifier: &[u8],
    redirect_uri: &str,
) -> Result<SecretTokenBody, OAuthPublicError> {
    let code = std::str::from_utf8(code)
        .map_err(|_| OAuthPublicError::new("invalid_authorization_code", false))?;
    let verifier = std::str::from_utf8(verifier)
        .map_err(|_| OAuthPublicError::new("invalid_pkce_verifier", false))?;
    let state = std::str::from_utf8(state)
        .map_err(|_| OAuthPublicError::new("invalid_oauth_state", false))?;
    let request = AuthorizationCodeRequest {
        grant_type: "authorization_code",
        code,
        state: registration
            .authorization_code_includes_state
            .then_some(state),
        client_id: &registration.client_id,
        redirect_uri,
        code_verifier: verifier,
    };
    match registration.authorization_code_encoding {
        OAuthTokenRequestEncoding::Form => {
            let mut encoded = url::form_urlencoded::Serializer::new(SecretFormBody::empty());
            encoded
                .append_pair("grant_type", request.grant_type)
                .append_pair("code", request.code);
            if let Some(state) = request.state {
                encoded.append_pair("state", state);
            }
            encoded
                .append_pair("client_id", request.client_id)
                .append_pair("redirect_uri", request.redirect_uri)
                .append_pair("code_verifier", request.code_verifier);
            Ok(SecretTokenBody::Form(encoded.finish()))
        }
        OAuthTokenRequestEncoding::Json => Ok(SecretTokenBody::Json(encode_secret_json(&request)?)),
    }
}

async fn exchange_authorization_code(
    client: &reqwest::Client,
    registration: &OAuthProviderRegistration,
    code: &[u8],
    state: &[u8],
    verifier: &[u8],
    nonce: &[u8],
    redirect_uri: &str,
) -> Result<OAuthTokenBundleV1, OAuthPublicError> {
    let body = authorization_code_request_body(registration, code, state, verifier, redirect_uri)?;
    let content_type = body.content_type();
    let response = client
        .post(registration.token_endpoint.clone())
        .header(reqwest::header::CONTENT_TYPE, content_type)
        // The response body must become exclusively owned after EOF so its
        // source buffers can be scrubbed. A token connection is deliberately
        // not pooled: hyper may otherwise retain a sibling slice of its read
        // buffer and make `Bytes::try_into_mut` fail.
        .header(reqwest::header::CONNECTION, "close")
        .body(reqwest::Body::from(bytes::Bytes::from_owner(body)))
        .send()
        .await
        .map_err(|_| OAuthPublicError::new("token_endpoint_unavailable", true))?;
    if response.status().is_redirection() {
        return Err(OAuthPublicError::new("token_redirect_rejected", false));
    }
    let status = response.status();
    let bytes = bounded_response(response).await?;
    if !status.is_success() {
        return Err(classify_token_error(status.as_u16(), &bytes));
    }
    token_bundle_from_response(registration, &bytes, nonce, 1, None).await
}

async fn bounded_response(
    mut response: reqwest::Response,
) -> Result<Zeroizing<Vec<u8>>, OAuthPublicError> {
    let declared_oversized = response
        .content_length()
        .is_some_and(|length| length > TOKEN_RESPONSE_LIMIT as u64);
    let mut source_chunks = Vec::new();
    let mut source_len = 0_usize;
    let mut oversized = false;
    let mut transport_failed = false;
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                source_len = source_len.saturating_add(chunk.len());
                source_chunks.push(chunk);
                if source_len > TOKEN_RESPONSE_LIMIT {
                    oversized = true;
                    break;
                }
            }
            Ok(None) => break,
            Err(_) => {
                transport_failed = true;
                break;
            }
        }
    }
    // Exhaustion (or dropping an oversized/incomplete body) closes the
    // Connection: close transport and releases hyper's non-secret sibling
    // slices. Exclusivity is EVENTUAL, not immediate: the connection task
    // may hold its read-buffer reference for a few more scheduler ticks, so
    // the scrub below waits for it instead of treating the race as an
    // invariant breach.
    drop(response);
    let mut bytes = Zeroizing::new(Vec::with_capacity(source_len.min(TOKEN_RESPONSE_LIMIT)));
    if !oversized {
        for chunk in &source_chunks {
            bytes.extend_from_slice(chunk.as_ref());
        }
    }
    scrub_source_chunks(source_chunks).await;
    if declared_oversized || oversized {
        return Err(OAuthPublicError::new("token_response_oversized", false));
    }
    if transport_failed {
        return Err(OAuthPublicError::new("token_endpoint_unavailable", true));
    }
    Ok(bytes)
}

/// Zeroize every token-bearing source chunk once its backing storage is
/// exclusively owned. `Bytes::try_into_mut` fails while any sibling
/// reference is alive — in practice hyper's connection task, which drops
/// its read-buffer reference a few scheduler ticks after the response —
/// so sweep with yields until every chunk scrubs. A chunk still shared at
/// the bound drops unscrubbed and is reported as the residual count: a
/// bounded in-process memory-hygiene residual, NEVER process death (the
/// previous `std::process::abort` here traded every live session for a
/// refcount race and killed the daemon under ordinary scheduler load).
async fn scrub_source_chunks(mut pending: Vec<bytes::Bytes>) -> usize {
    for _ in 0..SCRUB_YIELD_BOUND {
        pending = pending
            .into_iter()
            .filter_map(|chunk| match chunk.try_into_mut() {
                Ok(mut chunk) => {
                    chunk.as_mut().zeroize();
                    None
                }
                Err(chunk) => Some(chunk),
            })
            .collect();
        if pending.is_empty() {
            return 0;
        }
        tokio::task::yield_now().await;
    }
    pending.len()
}

/// The per-provider redirect shape. Claude Code parity (owner bug):
/// Anthropic's client allowlist accepts ONLY
/// `http://localhost:<port>/callback` — the hardened random path segment
/// is rejected with "Redirect URI … not supported by client". CSRF stays
/// covered by `state` + PKCE, and the per-flow PORT still discriminates
/// flows; every other provider keeps the hardened random-path shape.
///
/// The returned `authority` is the exact `host:port` a browser navigating
/// to `uri` sends as its `Host` header, and it is the ONLY authority the
/// flow's loopback listener accepts. The listener MUST validate against
/// this same composed instance — the v0.0.65 regression (owner screenshot)
/// moved the redirect to `localhost:<port>` while the listener kept
/// demanding `127.0.0.1:<port>`, so every legitimate Anthropic callback
/// carrying the CORRECT state was served the rejection page.
pub(crate) fn compose_redirect(
    provider: &str,
    port: u16,
    hardened_segment: &str,
) -> (String, String, String) {
    if provider == haider_provider::ANTHROPIC_OAUTH_PROVIDER_NAME {
        (
            "/callback".to_owned(),
            format!("http://localhost:{port}/callback"),
            format!("localhost:{port}"),
        )
    } else {
        let path = format!("/oauth/callback/{hardened_segment}");
        let uri = format!("http://127.0.0.1:{port}{path}");
        (path, uri, format!("127.0.0.1:{port}"))
    }
}

async fn bounded_jwks_response(
    response: reqwest::Response,
) -> Result<Zeroizing<Vec<u8>>, OAuthPublicError> {
    match bounded_response(response).await {
        Ok(bytes) => Ok(bytes),
        Err(error) if error.code == "token_response_oversized" => {
            Err(OAuthPublicError::new("identity_keys_malformed", true))
        }
        Err(_) => Err(OAuthPublicError::new("identity_verifier_unavailable", true)),
    }
}

fn classify_token_error(status: u16, body: &[u8]) -> OAuthPublicError {
    #[derive(Deserialize)]
    struct ErrorBody {
        #[serde(default)]
        error: Option<SecretJson>,
    }
    let kind = serde_json::from_slice::<ErrorBody>(body)
        .ok()
        .and_then(|value| value.error);
    if status == 401
        || status == 403
        || kind
            .as_ref()
            .is_some_and(|kind| kind.0.as_slice() == b"invalid_grant")
    {
        OAuthPublicError::new("invalid_grant", false)
    } else if status == 429 || status >= 500 {
        OAuthPublicError::retryable_status()
    } else {
        OAuthPublicError::new("token_exchange_failed", false)
    }
}

pub(crate) struct SecretJson(pub(crate) Zeroizing<Vec<u8>>);

impl<'de> Deserialize<'de> for SecretJson {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct SecretVisitor;
        impl Visitor<'_> for SecretVisitor {
            type Value = SecretJson;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a secret string")
            }

            fn visit_borrowed_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(SecretJson(Zeroizing::new(value.as_bytes().to_vec())))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                self.visit_borrowed_str(value)
            }

            fn visit_string<E>(self, mut value: String) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                use zeroize::Zeroize as _;
                let secret = Zeroizing::new(value.as_bytes().to_vec());
                value.zeroize();
                Ok(SecretJson(secret))
            }
        }
        deserializer.deserialize_str(SecretVisitor)
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: SecretJson,
    #[serde(default)]
    refresh_token: Option<SecretJson>,
    token_type: String,
    expires_in: u64,
    #[serde(default)]
    refresh_expires_in: Option<u64>,
    scope: String,
    #[serde(default)]
    id_token: Option<SecretJson>,
}

async fn token_bundle_from_response(
    registration: &OAuthProviderRegistration,
    bytes: &[u8],
    nonce: &[u8],
    generation: u64,
    prior_refresh: Option<&[u8]>,
) -> Result<OAuthTokenBundleV1, OAuthPublicError> {
    let response = serde_json::from_slice::<TokenResponse>(bytes)
        .map_err(|_| OAuthPublicError::new("malformed_token_response", false))?;
    if !response.token_type.eq_ignore_ascii_case("bearer")
        || response.expires_in == 0
        || response.expires_in > MAX_TOKEN_LIFETIME_SECS
        || response
            .refresh_expires_in
            .is_some_and(|expiry| expiry == 0 || expiry > MAX_TOKEN_LIFETIME_SECS)
    {
        return Err(OAuthPublicError::new("invalid_token_response", false));
    }
    let scopes = response
        .scope
        .split_ascii_whitespace()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    if !registration
        .scopes
        .iter()
        .filter(|scope| registration.validation_required(scope))
        .all(|scope| scopes.contains(scope))
    {
        return Err(OAuthPublicError::new("scope_mismatch", false));
    }
    let identity = match &registration.identity_mode {
        RuntimeIdentityMode::VerifiedIdToken(verifier) => {
            let id_token = response
                .id_token
                .as_ref()
                .ok_or_else(|| OAuthPublicError::new("missing_id_token", false))?;
            verifier
                .verify(
                    id_token.0.as_slice(),
                    OAuthIdentityExpectation {
                        issuer: &registration.issuer,
                        audience: &registration.audience,
                        nonce,
                    },
                )
                .await?
        }
        RuntimeIdentityMode::TokenEndpointGrant { display_identity } => OAuthIdentityV1 {
            subject_hash: blake3::hash(response.access_token.0.as_slice())
                .to_hex()
                .to_string(),
            display_identity: display_identity.clone(),
        },
    };
    let now = now_ms().ok_or_else(|| OAuthPublicError::new("clock_unavailable", true))?;
    let mut account_identity =
        match haider_provider::oauth_identity_source(&registration.provider_id) {
            Some(source) => source
                .identity_from_tokens(&haider_provider::OAuthTokens {
                    access_token: response.access_token.0.as_slice(),
                    refresh_token: response
                        .refresh_token
                        .as_ref()
                        .map(|token| token.0.as_slice())
                        .or(prior_refresh),
                    id_token: response.id_token.as_ref().map(|token| token.0.as_slice()),
                    captured_at: now,
                })
                .map_err(|_| OAuthPublicError::new("identity_claims_malformed", false))?,
            None => None,
        };
    if matches!(
        &registration.identity_mode,
        RuntimeIdentityMode::VerifiedIdToken(_)
    ) && let Some(account_identity) = account_identity.as_mut()
    {
        // The generic decoder is informational; this bit is promoted only
        // because the flow independently verified this exact ID token.
        account_identity.verified = true;
    }
    let expires_at = now
        .checked_add(response.expires_in.saturating_mul(1000))
        .ok_or_else(|| OAuthPublicError::new("invalid_token_response", false))?;
    let refresh_expires_at = response
        .refresh_expires_in
        .and_then(|seconds| now.checked_add(seconds.saturating_mul(1000)));
    let refresh_token = match response.refresh_token {
        Some(token) => Some(token.0),
        None if registration.retain_refresh_on_omission => {
            prior_refresh.map(|token| Zeroizing::new(token.to_vec()))
        }
        None if registration.refresh_policy == OAuthRefreshPolicy::SerializedRotating => {
            return Err(OAuthPublicError::new("missing_refresh_token", false));
        }
        None => None,
    };
    let id_token = response.id_token.map(|token| token.0);
    let mut bundle = OAuthTokenBundleV1::new(
        registration.provider_id.clone(),
        registration.issuer.clone(),
        registration.audience.clone(),
        registration.resource.clone(),
        response.token_type,
        response.access_token.0,
        refresh_token,
        expires_at,
        refresh_expires_at,
        scopes.into_iter().collect(),
        identity,
        generation,
    )
    .map_err(|_| OAuthPublicError::new("invalid_token_response", false))?;
    if let Some(identity) = account_identity {
        bundle = bundle.with_account_identity(identity);
    }
    if let Some(id_token) = id_token {
        bundle = bundle.with_id_token(id_token);
    }
    Ok(
        if registration.refresh_policy == OAuthRefreshPolicy::SerializedRotating {
            bundle.with_refresh_after(kimi_refresh_after(now, response.expires_in))
        } else {
            bundle
        },
    )
}

fn kimi_refresh_after(issued_at_unix_ms: u64, expires_in_secs: u64) -> u64 {
    let threshold_secs = 300_u64.max(expires_in_secs / 2);
    issued_at_unix_ms.saturating_add(
        expires_in_secs
            .saturating_sub(threshold_secs)
            .saturating_mul(1000),
    )
}

fn random_secret(length: usize) -> Result<Zeroizing<Vec<u8>>, OAuthPublicError> {
    let mut bytes = Zeroizing::new(vec![0_u8; length]);
    getrandom::fill(bytes.as_mut_slice())
        .map_err(|_| OAuthPublicError::new("randomness_unavailable", true))?;
    Ok(bytes)
}

fn random_id(prefix: &str, counter: u64) -> Result<String, OAuthPublicError> {
    let bytes = random_secret(16)?;
    Ok(format!(
        "{prefix}-{counter:x}-{}",
        URL_SAFE_NO_PAD.encode(bytes.as_slice())
    ))
}

fn set_terminal(inner: &CoordinatorInner, flow_id: &OAuthFlowId, status: InternalFlowStatus) {
    if let Ok(mut flows) = inner.flows.lock()
        && let Some(flow) = flows.get_mut(flow_id.as_str())
    {
        flow.status = status;
    }
}

fn expire_if_needed(flow: &mut FlowEntry) {
    if Instant::now() >= flow.deadline
        && !matches!(
            flow.status,
            InternalFlowStatus::Failed(_)
                | InternalFlowStatus::Expired
                | InternalFlowStatus::Cancelled
        )
    {
        flow.cancel.send_replace(true);
        flow.status = InternalFlowStatus::Expired;
    }
}

fn connection_is_active(inner: &CoordinatorInner, connection_id: &str) -> bool {
    !inner.shutting_down.load(Ordering::Acquire)
        && inner
            .active_connections
            .lock()
            .is_ok_and(|active| active.contains(connection_id))
}

fn reserve_flow_capacity(flows: &mut HashMap<String, FlowEntry>, max_flows: usize) -> bool {
    let now = Instant::now();
    flows.retain(|_, flow| flow.deadline > now);
    if flows.len() < max_flows {
        return true;
    }
    let oldest_terminal = flows
        .iter()
        .filter(|(_, flow)| {
            matches!(
                flow.status,
                InternalFlowStatus::Failed(_)
                    | InternalFlowStatus::Expired
                    | InternalFlowStatus::Cancelled
            )
        })
        .min_by_key(|(_, flow)| flow.deadline)
        .map(|(flow_id, _)| flow_id.clone());
    if let Some(flow_id) = oldest_terminal {
        flows.remove(&flow_id);
    }
    flows.len() < max_flows
}

fn public_status(flow: &FlowEntry) -> OAuthFlowStatusWire {
    match &flow.status {
        InternalFlowStatus::WaitingBrowser => OAuthFlowStatusWire::WaitingBrowser,
        InternalFlowStatus::WaitingDevice => OAuthFlowStatusWire::WaitingDevice,
        InternalFlowStatus::Exchanging => OAuthFlowStatusWire::Exchanging,
        InternalFlowStatus::Ready { reference, bundle } => OAuthFlowStatusWire::Ready {
            oauth_reference: OAuthReadyRefWire::new(reference.as_str()),
            identity: bundle.identity.display_identity.clone(),
            expires_at_ms: bundle.expires_at_unix_ms,
        },
        InternalFlowStatus::Failed(public_code) => OAuthFlowStatusWire::Failed {
            public_code: (*public_code).into(),
        },
        InternalFlowStatus::Expired => OAuthFlowStatusWire::Expired,
        InternalFlowStatus::Cancelled => OAuthFlowStatusWire::Cancelled,
    }
}

fn owns(flow: &FlowEntry, instance_id: &str, connection_id: &str, attempt_id: &str) -> bool {
    flow.owner.daemon_instance == instance_id
        && flow.owner.connection_id == connection_id
        && flow.attempt_id == attempt_id
}

fn valid_alias(alias: &str) -> bool {
    let bytes = alias.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
}

fn safe_url(url: &Url) -> String {
    let host = url.host_str().unwrap_or("invalid");
    match url.port() {
        Some(port) => format!("{}://{host}:{port}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    }
}

fn now_ms() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn respond(route: &OAuthRoute, body: ResponseBody) {
    if route
        .sink
        .try_send(WireFrame::Response {
            request_id: route.request_id.clone(),
            body,
        })
        .is_err()
    {
        route.sink.close_after_required_delivery_failure();
    }
}

fn respond_error(route: &OAuthRoute, code: &str, message: &str, retryable: bool) {
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

fn respond_public_error(route: &OAuthRoute, error: OAuthPublicError) {
    respond_error(
        route,
        error.code,
        "OAuth flow could not be started",
        error.retryable,
    );
}

/// Branded success page. A static constant: no request byte can reach it.
const SUCCESS_HTML: &str = concat!(
    callback_page_head!(),
    "<title>Haider — authorization complete</title>",
    callback_page_style!(),
    callback_page_wordmark!(),
    "<h1>Authorization received</h1>",
    "<p>You can close this tab and return to Haider.</p>",
    "</main></html>"
);

/// Branded cancellation page. A static constant: no request byte can reach it.
const DENIED_HTML: &str = concat!(
    callback_page_head!(),
    "<title>Haider — authorization cancelled</title>",
    callback_page_style!(),
    callback_page_wordmark!(),
    "<h1>Authorization was not granted</h1>",
    "<p>Return to Haider to start the sign-in again whenever you like.</p>",
    "</main></html>"
);

async fn send_callback_page(
    stream: &mut TcpStream,
    status: u16,
    html: &str,
) -> std::io::Result<()> {
    let reason = if status == 200 { "OK" } else { "Bad Request" };
    let headers = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nPragma: no-cache\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n\r\n",
        html.len()
    );
    stream.write_all(headers.as_bytes()).await?;
    stream.write_all(html.as_bytes()).await?;
    stream.shutdown().await
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RefreshKey {
    provider: String,
    alias: String,
    generation: u64,
    issuer: String,
    subject_hash: String,
    fence_epoch: u64,
}

#[derive(Debug, Clone)]
enum RefreshFlightOutcome {
    Refreshed,
    Imported(crate::accounts::OAuthRefreshFence),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SerializedRefreshRecovery {
    SignInAgain,
    ReimportCodex,
}

struct RefreshFlight {
    completed: Mutex<Option<Result<RefreshFlightOutcome, HaiderError>>>,
    notify: Notify,
}

impl RefreshFlight {
    fn new() -> Self {
        Self {
            completed: Mutex::new(None),
            notify: Notify::new(),
        }
    }

    fn finish(&self, result: Result<RefreshFlightOutcome, HaiderError>) {
        if let Ok(mut completed) = self.completed.lock() {
            *completed = Some(result);
        }
        self.notify.notify_waiters();
    }

    async fn wait(&self) -> Result<RefreshFlightOutcome, HaiderError> {
        loop {
            let notified = self.notify.notified();
            if let Ok(completed) = self.completed.lock()
                && let Some(result) = completed.clone()
            {
                return result;
            }
            notified.await;
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct RefreshFenceRegistry {
    fences: Arc<Mutex<HashMap<String, Arc<AtomicU64>>>>,
}

impl RefreshFenceRegistry {
    fn fence_for(&self, alias: &CredentialAlias) -> Arc<AtomicU64> {
        self.fences
            .lock()
            .map(|mut fences| {
                Arc::clone(
                    fences
                        .entry(alias.as_str().to_owned())
                        .or_insert_with(|| Arc::new(AtomicU64::new(0))),
                )
            })
            .unwrap_or_else(|_| Arc::new(AtomicU64::new(u64::MAX)))
    }

    pub(crate) fn current(&self, alias: &CredentialAlias) -> u64 {
        self.fence_for(alias).load(Ordering::Acquire)
    }

    pub(crate) fn invalidate(&self, alias: &CredentialAlias) {
        self.fence_for(alias).fetch_add(1, Ordering::AcqRel);
    }
}

struct BrokerInner {
    vault: Arc<dyn Vault>,
    catalog: OAuthProviderCatalog,
    snapshot: crate::accounts::AccountsSnapshot,
    status_commands: mpsc::Sender<crate::accounts::AccountCommand>,
    flights: Mutex<HashMap<RefreshKey, Arc<RefreshFlight>>>,
    fences: RefreshFenceRegistry,
    shutting_down: AtomicBool,
    client: reqwest::Client,
    refresh_exchange: Arc<dyn OAuthRefreshExchange>,
    refresh_skew: Duration,
    /// G4b (LV2): the mockable `gcloud auth print-access-token` source the
    /// vertex gcloud-refresh credential re-mints through on auth failure.
    gcloud: Arc<dyn crate::gcloud::GcloudAccessTokenSource>,
    #[cfg(test)]
    panic_refresh_worker: AtomicBool,
}

#[async_trait::async_trait]
pub(crate) trait OAuthRefreshExchange: Send + Sync {
    async fn exchange(
        &self,
        client: &reqwest::Client,
        registration: &OAuthProviderRegistration,
        refresh_token: &[u8],
        device_id: Option<&SecretHandle>,
    ) -> Result<Zeroizing<Vec<u8>>, OAuthPublicError>;
}

struct ProductionOAuthRefreshExchange;

#[async_trait::async_trait]
impl OAuthRefreshExchange for ProductionOAuthRefreshExchange {
    async fn exchange(
        &self,
        client: &reqwest::Client,
        registration: &OAuthProviderRegistration,
        refresh_token: &[u8],
        device_id: Option<&SecretHandle>,
    ) -> Result<Zeroizing<Vec<u8>>, OAuthPublicError> {
        exchange_refresh_token(client, registration, refresh_token, device_id).await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SnapshotOAuthState {
    Current,
    Expired,
    Replaced,
}

/// Completes the public single-flight even if the worker unwinds before its
/// normal `finish` call. `JoinSet` still records the panic for shutdown
/// honesty; this guard is the immediate fail-closed wakeup for waiters.
struct RefreshWorkerCompletion {
    broker: Weak<BrokerInner>,
    task_owner: Weak<OwnedTaskSet>,
    key: RefreshKey,
    flight: Arc<RefreshFlight>,
    armed: bool,
}

impl RefreshWorkerCompletion {
    fn new(
        broker: Weak<BrokerInner>,
        task_owner: Weak<OwnedTaskSet>,
        key: RefreshKey,
        flight: Arc<RefreshFlight>,
    ) -> Self {
        Self {
            broker,
            task_owner,
            key,
            flight,
            armed: true,
        }
    }

    fn finish(mut self, result: Result<RefreshFlightOutcome, HaiderError>) {
        self.flight.finish(result);
        self.remove_registered_flight();
        self.armed = false;
    }

    fn remove_registered_flight(&self) {
        let Some(broker) = self.broker.upgrade() else {
            return;
        };
        let mut flights = broker
            .flights
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if flights
            .get(&self.key)
            .is_some_and(|current| Arc::ptr_eq(current, &self.flight))
        {
            flights.remove(&self.key);
        }
    }
}

impl Drop for RefreshWorkerCompletion {
    fn drop(&mut self) {
        if self.armed {
            // Seal task admission before either removing the failed flight or
            // waking its waiters. Otherwise a resolver woken during panic
            // unwind can register replacement work before JoinSet observes
            // the panic and seals its owner.
            if let Some(task_owner) = self.task_owner.upgrade() {
                task_owner.seal();
            }
            self.remove_registered_flight();
            self.flight.finish(Err(refresh_worker_failed()));
        }
    }
}

/// Owns the interval between publishing a new flight and registering its
/// daemon-owned task. Cancellation of the resolver during `OwnedTaskSet::spawn`
/// drops this guard, so no mapped flight can survive without a live task.
struct RefreshFlightRegistration {
    broker: Weak<BrokerInner>,
    key: RefreshKey,
    flight: Arc<RefreshFlight>,
    armed: bool,
}

impl RefreshFlightRegistration {
    fn new(broker: Weak<BrokerInner>, key: RefreshKey, flight: Arc<RefreshFlight>) -> Self {
        Self {
            broker,
            key,
            flight,
            armed: true,
        }
    }

    fn commit(&mut self) {
        self.armed = false;
    }

    fn remove_registered_flight(&self) {
        let Some(broker) = self.broker.upgrade() else {
            return;
        };
        let mut flights = broker
            .flights
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if flights
            .get(&self.key)
            .is_some_and(|current| Arc::ptr_eq(current, &self.flight))
        {
            flights.remove(&self.key);
        }
    }
}

impl Drop for RefreshFlightRegistration {
    fn drop(&mut self) {
        if self.armed {
            self.remove_registered_flight();
            self.flight.finish(Err(refresh_worker_failed()));
        }
    }
}

fn poison_refresh_flights(inner: &BrokerInner) {
    let flights = inner
        .flights
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .drain()
        .map(|(_, flight)| flight)
        .collect::<Vec<_>>();
    for flight in flights {
        flight.finish(Err(refresh_worker_failed()));
    }
}

/// Auth-aware credential broker used by provider construction.
///
/// API-key descriptors resolve their vault value unchanged. OAuth descriptors
/// decode the versioned bundle and return only its access token. Refresh is
/// keyed by `(provider, alias, generation)`, persisted before release, and
/// fenced against removal/replacement.
#[derive(Clone)]
pub(crate) struct CredentialBroker {
    inner: Arc<BrokerInner>,
    tasks: Arc<OwnedTaskSet>,
}

impl CredentialBroker {
    #[cfg(test)]
    pub(crate) fn new(
        vault: Arc<dyn Vault>,
        catalog: OAuthProviderCatalog,
        snapshot: crate::accounts::AccountsSnapshot,
        status_commands: mpsc::Sender<crate::accounts::AccountCommand>,
    ) -> Result<Self, HaiderError> {
        Self::new_with_fences(
            vault,
            catalog,
            snapshot,
            status_commands,
            RefreshFenceRegistry::default(),
        )
    }

    pub(crate) fn new_with_fences(
        vault: Arc<dyn Vault>,
        catalog: OAuthProviderCatalog,
        snapshot: crate::accounts::AccountsSnapshot,
        status_commands: mpsc::Sender<crate::accounts::AccountCommand>,
        fences: RefreshFenceRegistry,
    ) -> Result<Self, HaiderError> {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(TOKEN_TIMEOUT)
            .build()
            .map_err(|_| {
                HaiderError::new(
                    ErrorCode::ProviderError,
                    "OAuth refresh transport is unavailable",
                    true,
                )
            })?;
        Ok(Self {
            inner: Arc::new(BrokerInner {
                vault,
                catalog,
                snapshot,
                status_commands,
                flights: Mutex::new(HashMap::new()),
                fences,
                shutting_down: AtomicBool::new(false),
                client,
                refresh_exchange: Arc::new(ProductionOAuthRefreshExchange),
                refresh_skew: OAUTH_REFRESH_SKEW,
                gcloud: Arc::new(crate::gcloud::GcloudCli),
                #[cfg(test)]
                panic_refresh_worker: AtomicBool::new(false),
            }),
            tasks: Arc::new(OwnedTaskSet::new()),
        })
    }

    /// Replaces the gcloud shell-out source. MUST run before the broker is
    /// shared (construction time); a shared broker is refused so a test can
    /// never silently keep the production CLI source.
    pub(crate) fn with_gcloud_source(
        mut self,
        gcloud: Arc<dyn crate::gcloud::GcloudAccessTokenSource>,
    ) -> Result<Self, HaiderError> {
        let Some(inner) = Arc::get_mut(&mut self.inner) else {
            return Err(HaiderError::new(
                ErrorCode::Internal,
                "gcloud source must be installed before the broker is shared",
                false,
            ));
        };
        inner.gcloud = gcloud;
        Ok(self)
    }

    /// Replaces the refresh transport for hermetic tests. Like the gcloud
    /// seam, installation is construction-only so a shared broker can never
    /// retain a production exchanger accidentally.
    #[cfg(test)]
    pub(crate) fn with_refresh_exchange(
        mut self,
        refresh_exchange: Arc<dyn OAuthRefreshExchange>,
    ) -> Result<Self, HaiderError> {
        let Some(inner) = Arc::get_mut(&mut self.inner) else {
            return Err(HaiderError::new(
                ErrorCode::Internal,
                "OAuth refresh exchange must be installed before the broker is shared",
                false,
            ));
        };
        inner.refresh_exchange = refresh_exchange;
        Ok(self)
    }

    pub(crate) async fn resolve(
        &self,
        descriptor: &CredentialDescriptor,
    ) -> Result<SecretHandle, HaiderError> {
        if self.inner.shutting_down.load(Ordering::Acquire) {
            return Err(broker_stopped());
        }
        match descriptor.auth_method {
            AuthMethod::ApiKey => self.resolve_vault(&descriptor.alias).await,
            AuthMethod::OAuth => match Self::snapshot_oauth_state(&self.inner, descriptor) {
                SnapshotOAuthState::Current | SnapshotOAuthState::Expired => {
                    self.resolve_oauth(descriptor, false, None).await
                }
                SnapshotOAuthState::Replaced => {
                    let Some(flight) = self.active_flight(descriptor) else {
                        return Err(expired_or_replaced(descriptor));
                    };
                    match flight.wait().await? {
                        RefreshFlightOutcome::Imported(expected) => {
                            self.resolve_imported_bundle(descriptor, &expected).await
                        }
                        RefreshFlightOutcome::Refreshed
                            if Self::snapshot_oauth_state(&self.inner, descriptor)
                                != SnapshotOAuthState::Replaced =>
                        {
                            self.resolve_oauth(descriptor, false, None).await
                        }
                        RefreshFlightOutcome::Refreshed => Err(expired_or_replaced(descriptor)),
                    }
                }
            },
        }
    }

    pub(crate) async fn refresh_after_auth_failure(
        &self,
        descriptor: &CredentialDescriptor,
        failed_access_fingerprint: Option<[u8; 32]>,
    ) -> Result<SecretHandle, HaiderError> {
        match descriptor.auth_method {
            AuthMethod::OAuth => {
                self.resolve_oauth(descriptor, true, failed_access_fingerprint)
                    .await
            }
            // G4b (LV2): the vertex gcloud-refresh credential re-mints
            // through the mockable shell-out — a fresh token is vaulted
            // before the turn retries; failure surfaces the typed gcloud
            // error instead of a generic auth failure.
            AuthMethod::ApiKey if crate::gcloud::is_gcloud_refresh_descriptor(descriptor) => {
                self.refresh_gcloud(descriptor).await
            }
            AuthMethod::ApiKey => Err(rotation_error(
                descriptor,
                haider_accounts::RotationTrigger::AuthExpired,
                false,
                "API credential authentication failed",
            )),
        }
    }

    /// Runs the gcloud source off the async runtime, vaults the fresh token
    /// under the SAME alias, and returns the re-read handle. The vault write
    /// is the durable truth (the transcription-secret precedent): the next
    /// resolve serves the refreshed token with no descriptor mutation.
    async fn refresh_gcloud(
        &self,
        descriptor: &CredentialDescriptor,
    ) -> Result<SecretHandle, HaiderError> {
        let gcloud = Arc::clone(&self.inner.gcloud);
        let token = tokio::task::spawn_blocking(move || gcloud.print_access_token())
            .await
            .map_err(|_| crate::gcloud::gcloud_error("refresh worker was lost"))??;
        let vault = Arc::clone(&self.inner.vault);
        let alias = descriptor.alias.clone();
        tokio::task::spawn_blocking(move || {
            vault
                .put(&alias, &token)
                .and_then(|()| vault.resolve(&alias))
        })
        .await
        .map_err(|_| crate::gcloud::gcloud_error("vault worker was lost"))?
    }

    pub(crate) async fn resolve_account(
        &self,
        provider: &str,
        failure: Option<(CredentialAlias, haider_accounts::RotationTrigger)>,
    ) -> Result<crate::accounts::ResolvedAccount, HaiderError> {
        let (completed, result) = oneshot::channel();
        self.inner
            .status_commands
            .send(crate::accounts::AccountCommand::ResolveCredential {
                provider: provider.to_owned(),
                failure,
                completed,
            })
            .await
            .map_err(|_| {
                HaiderError::new(
                    ErrorCode::ProviderError,
                    "account resolver service is unavailable",
                    true,
                )
            })?;
        result.await.map_err(|_| {
            HaiderError::new(
                ErrorCode::ProviderError,
                "account resolver service dropped its response",
                true,
            )
        })?
    }

    async fn resolve_oauth(
        &self,
        descriptor: &CredentialDescriptor,
        force_refresh: bool,
        failed_access_fingerprint: Option<[u8; 32]>,
    ) -> Result<SecretHandle, HaiderError> {
        let fence = self.fence_for(&descriptor.alias);
        let expected_fence = fence.load(Ordering::Acquire);
        let (bundle, source_first, refresh_allowed) = loop {
            let stored = self.resolve_vault(&descriptor.alias).await?;
            let bundle = OAuthTokenBundleV1::decode(stored.expose_secret())?;
            if fence.load(Ordering::Acquire) != expected_fence {
                return Err(stale_refresh());
            }
            if let Err(error) = self.validate_bundle(descriptor, &bundle) {
                let _ =
                    Self::mark_expired_if_current(&self.inner, descriptor, &bundle, expected_fence)
                        .await?;
                return Err(error);
            }
            let now = now_ms().ok_or_else(|| {
                HaiderError::new(ErrorCode::Internal, "system clock is unavailable", true)
            })?;
            let registration_serialized_rotating =
                registration_is_serialized_rotating(&self.inner.catalog, &descriptor.provider);
            // An active rejection/uncertainty marker is NOT terminal at this
            // pre-lease read. A live concurrent refresher persists permanent
            // uncertainty while it still holds the physical vault-alias lease
            // and only then performs the rotating request, so the marker this
            // broker just observed may be superseded by that refresher's
            // durable rotation moments later. Route into the serialized
            // refresh path instead: its under-lease re-read either adopts the
            // rotated bundle or surfaces the typed re-login, and it never
            // replays a rejected token. Only that re-read may treat the
            // marker as terminal.
            let rejection_marker_active = bundle
                .refresh_rejected_until_unix_ms
                .is_some_and(|until| now < until);
            if force_refresh
                && (registration_serialized_rotating
                    || descriptor.provider == haider_provider::OPENAI_OAUTH_PROVIDER_NAME)
                && !rejection_marker_active
                && failed_access_fingerprint.is_some_and(|failed| {
                    !bool::from(
                        blake3::hash(bundle.access_token())
                            .as_bytes()
                            .ct_eq(&failed),
                    )
                })
                && bundle.expires_at_unix_ms > now
            {
                // The 401 belongs to an older access generation. Another
                // process already rotated and persisted before this broker
                // entered; adopt the under-vault re-read instead of rotating
                // the fresh generation again.
                return Ok(bundle.access_token_handle());
            }
            let skew_ms = duration_ms(self.inner.refresh_skew);
            let snapshot_state = Self::snapshot_oauth_state(&self.inner, descriptor);
            if matches!(
                snapshot_state,
                SnapshotOAuthState::Expired | SnapshotOAuthState::Replaced
            ) && let Some(flight) = self.active_flight(descriptor)
            {
                match flight.wait().await? {
                    RefreshFlightOutcome::Imported(expected) => {
                        return self.resolve_imported_bundle(descriptor, &expected).await;
                    }
                    RefreshFlightOutcome::Refreshed
                        if Self::snapshot_oauth_state(&self.inner, descriptor)
                            == SnapshotOAuthState::Current =>
                    {
                        continue;
                    }
                    RefreshFlightOutcome::Refreshed => {
                        return Err(expired_or_replaced(descriptor));
                    }
                }
            }
            let actually_expired = bundle.expires_at_unix_ms <= now;
            let refresh_due = if registration_serialized_rotating {
                // The marker forces the serialized path even when the access
                // token is otherwise usable: an uncertain/rejected bundle must
                // resolve through the lease, never straight from the vault.
                rejection_marker_active
                    || bundle
                        .refresh_after_unix_ms
                        .is_none_or(|refresh_after| now >= refresh_after)
            } else {
                let proactive_skew_ms =
                    if descriptor.provider == haider_provider::GROK_OAUTH_PROVIDER_NAME {
                        duration_ms(Duration::from_secs(5 * 60))
                    } else {
                        skew_ms
                    };
                rejection_marker_active
                    || bundle.expires_at_unix_ms <= now.saturating_add(proactive_skew_ms)
            };
            if force_refresh
                || refresh_due
                || codex_import_fallback_refresh_candidate(&bundle, now)
                || snapshot_state == SnapshotOAuthState::Expired
            {
                // A snapshot-EXPIRED mark may record an UNCERTAIN refresh
                // (forced shutdown mid-exchange): the rotating refresh
                // token must never be replayed then. Source-first healing
                // never touches that token, so it stays allowed; the
                // refresh fallback is only for expiry we WITNESSED in
                // this process (skew/force/plain expiry, never the mark).
                break (
                    bundle,
                    actually_expired
                        || snapshot_state == SnapshotOAuthState::Expired
                        // C2 decision: the OpenAI registration stays
                        // Conservative so loopback-PKCE remains byte/behavior
                        // compatible. Every OpenAI refresh boundary asks the
                        // account actor for durable import provenance; only a
                        // current Codex import receives rotating-token
                        // serialization below.
                        || descriptor.provider == haider_provider::OPENAI_OAUTH_PROVIDER_NAME
                        // Claude Code owns native-store imports. Every
                        // Anthropic refresh/auth-failure boundary must ask
                        // the actor whether read-through is required before
                        // the independent grant fallback is even eligible.
                        || descriptor.provider == haider_provider::ANTHROPIC_OAUTH_PROVIDER_NAME,
                    snapshot_state != SnapshotOAuthState::Expired,
                );
            }
            match snapshot_state {
                SnapshotOAuthState::Current => return Ok(bundle.access_token_handle()),
                SnapshotOAuthState::Expired => unreachable!("expired OAuth broke for healing"),
                SnapshotOAuthState::Replaced => return Err(expired_or_replaced(descriptor)),
            }
        };
        let key = RefreshKey {
            provider: descriptor.provider.clone(),
            alias: descriptor.alias.as_str().to_owned(),
            generation: bundle.generation,
            issuer: bundle.issuer.clone(),
            subject_hash: bundle.identity.subject_hash.clone(),
            fence_epoch: expected_fence,
        };
        let (flight, leader) = {
            let mut flights = self
                .inner
                .flights
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(flight) = flights.get(&key) {
                (Arc::clone(flight), false)
            } else {
                let flight = Arc::new(RefreshFlight::new());
                flights.insert(key.clone(), Arc::clone(&flight));
                (flight, true)
            }
        };
        if leader {
            // The refresh is daemon-owned, not caller-owned. A cancelled
            // provider request must not abandon the flight, strand waiters,
            // or allow another refresh while a blocking vault write is still
            // completing.
            let broker = Arc::clone(&self.inner);
            let descriptor = descriptor.clone();
            let flight_for_worker = Arc::clone(&flight);
            let key_for_worker = key.clone();
            let task_owner = Arc::downgrade(&self.tasks);
            let mut registration = RefreshFlightRegistration::new(
                Arc::downgrade(&self.inner),
                key.clone(),
                Arc::clone(&flight),
            );
            if self
                .tasks
                .spawn(async move {
                    let completion = RefreshWorkerCompletion::new(
                        Arc::downgrade(&broker),
                        task_owner,
                        key_for_worker,
                        flight_for_worker,
                    );
                    #[cfg(test)]
                    if broker.panic_refresh_worker.swap(false, Ordering::AcqRel) {
                        panic!("injected OAuth refresh worker panic before completion");
                    }
                    let result = Self::heal_or_refresh(
                        &broker,
                        &descriptor,
                        &bundle,
                        expected_fence,
                        source_first,
                        refresh_allowed,
                    )
                    .await;
                    // All waiters observe the same effective outcome. A transient
                    // refresh failure may keep using an access token that has not
                    // actually expired.
                    let public_result = match result {
                        Ok(outcome) => Ok(outcome),
                        Err(error)
                            if !force_refresh
                                && error.retryable
                                && now_ms()
                                    .is_some_and(|current| bundle.expires_at_unix_ms > current) =>
                        {
                            Ok(RefreshFlightOutcome::Refreshed)
                        }
                        Err(error) => Err(error),
                    };
                    completion.finish(public_result);
                })
                .await
            {
                registration.commit();
            }
        }
        let outcome = flight.wait().await?;
        if let RefreshFlightOutcome::Imported(expected) = outcome {
            return self.resolve_imported_bundle(descriptor, &expected).await;
        }
        if !Self::snapshot_allows_oauth(&self.inner, descriptor) {
            return Err(rotation_error(
                descriptor,
                haider_accounts::RotationTrigger::AuthExpired,
                false,
                "OAuth credential is expired or was replaced",
            ));
        }
        let stored = self.resolve_vault(&descriptor.alias).await?;
        let refreshed = match OAuthTokenBundleV1::decode(stored.expose_secret()) {
            Ok(bundle) => bundle,
            Err(error) => return Err(error),
        };
        if fence.load(Ordering::Acquire) != expected_fence {
            return Err(stale_refresh());
        }
        if let Err(error) = self.validate_bundle(descriptor, &refreshed) {
            let _ =
                Self::mark_expired_if_current(&self.inner, descriptor, &refreshed, expected_fence)
                    .await?;
            return Err(error);
        }
        if now_ms().is_none_or(|current| refreshed.expires_at_unix_ms <= current) {
            return Err(HaiderError::new(
                ErrorCode::ProviderError,
                "OAuth access token expired while refresh was unavailable",
                true,
            ));
        }
        Ok(refreshed.access_token_handle())
    }

    async fn heal_or_refresh(
        inner: &Arc<BrokerInner>,
        descriptor: &CredentialDescriptor,
        bundle: &OAuthTokenBundleV1,
        expected_fence: u64,
        source_first: bool,
        refresh_allowed: bool,
    ) -> Result<RefreshFlightOutcome, HaiderError> {
        let mut refresh_descriptor = descriptor.clone();
        if source_first {
            refresh_descriptor.status = CredentialStatus::Ok;
            let (completed, result) = oneshot::channel();
            inner
                .status_commands
                .send(crate::accounts::AccountCommand::BeginOAuthImportHeal {
                    descriptor: descriptor.clone(),
                    expected: refresh_fence(bundle, expected_fence),
                    completed,
                })
                .await
                .map_err(|_| {
                    HaiderError::new(
                        ErrorCode::ProviderError,
                        "OAuth account actor is unavailable before import self-heal",
                        false,
                    )
                })?;
            let healed = result.await.map_err(|_| {
                HaiderError::new(
                    ErrorCode::ProviderError,
                    "OAuth import self-heal completion was lost",
                    false,
                )
            })??;
            match healed {
                crate::accounts::OAuthImportHealResult::Committed { expected } => {
                    return Ok(RefreshFlightOutcome::Imported(expected));
                }
                crate::accounts::OAuthImportHealResult::RefreshFallback { source } => {
                    // Under an UNCERTAIN mark (forced shutdown mid-refresh)
                    // the rotating refresh token must not be replayed — the
                    // stale source cannot heal, so the account stays down
                    // with its named remedy.
                    if !refresh_allowed {
                        return Err(imported_credential_expired(descriptor, &source));
                    }
                    let imported_codex = source == "codex"
                        && descriptor.provider == haider_provider::OPENAI_OAUTH_PROVIDER_NAME;
                    return Self::refresh(
                        inner,
                        &refresh_descriptor,
                        bundle,
                        expected_fence,
                        imported_codex,
                    )
                    .await
                    .map(|_| RefreshFlightOutcome::Refreshed)
                    .map_err(|error| imported_refresh_error(error, descriptor, &source));
                }
                crate::accounts::OAuthImportHealResult::LiveOwnerStore { source } => {
                    // The external app owns this rotating credential. The
                    // actor has either persisted the successfully re-read
                    // snapshot or observed a newer durable generation. Re-read
                    // Haider's vault; never spend its rotating refresh token.
                    let _ = source;
                    return Ok(RefreshFlightOutcome::Refreshed);
                }
                crate::accounts::OAuthImportHealResult::LiveOwnerUnavailable { failure } => {
                    return Err(claude_native_access_error(failure));
                }
                crate::accounts::OAuthImportHealResult::NotImported => {}
            }
        }
        if !refresh_allowed {
            // Not an import, mark possibly uncertain: the pre-heal law
            // stands — refuse rather than risk a double-spent rotation.
            return Err(expired_or_replaced(descriptor));
        }
        Self::refresh(inner, &refresh_descriptor, bundle, expected_fence, false)
            .await
            .map(|_| RefreshFlightOutcome::Refreshed)
    }

    async fn resolve_imported_bundle(
        &self,
        descriptor: &CredentialDescriptor,
        expected: &crate::accounts::OAuthRefreshFence,
    ) -> Result<SecretHandle, HaiderError> {
        if self.fence_for(&descriptor.alias).load(Ordering::Acquire) != expected.fence_epoch {
            return Err(stale_refresh());
        }
        let current = Self::current_oauth_descriptor(&self.inner, descriptor)
            .ok_or_else(|| expired_or_replaced(descriptor))?;
        let stored = self.resolve_vault(&descriptor.alias).await?;
        let bundle = OAuthTokenBundleV1::decode(stored.expose_secret())?;
        if bundle.generation != expected.generation
            || bundle.issuer != expected.issuer
            || bundle.audience != expected.audience
            || bundle.resource != expected.resource
            || bundle.identity.subject_hash != expected.subject_hash
        {
            return Err(stale_refresh());
        }
        self.validate_bundle(&current, &bundle)?;
        if now_ms().is_none_or(|now| bundle.expires_at_unix_ms <= now) {
            return Err(imported_credential_expired_for_provider(
                descriptor,
                &descriptor.provider,
            ));
        }
        Ok(bundle.access_token_handle())
    }

    async fn refresh(
        inner: &Arc<BrokerInner>,
        descriptor: &CredentialDescriptor,
        bundle: &OAuthTokenBundleV1,
        expected_fence: u64,
        imported_codex: bool,
    ) -> Result<SecretHandle, HaiderError> {
        let fence = Self::fence_for_inner(inner, &descriptor.alias);
        if fence.load(Ordering::Acquire) != expected_fence {
            return Err(stale_refresh());
        }
        let Some(registration) = inner.catalog.registration(&descriptor.provider) else {
            return Err(rotation_error(
                descriptor,
                haider_accounts::RotationTrigger::RefreshFailed,
                false,
                "OAuth registration is unavailable",
            ));
        };
        let active_serialized_marker = bundle
            .refresh_rejected_until_unix_ms
            .is_some_and(|until| now_ms().is_none_or(|now| now < until));
        if registration.refresh_policy == OAuthRefreshPolicy::SerializedRotating
            || imported_codex
            || active_serialized_marker
        {
            let recovery = if imported_codex {
                SerializedRefreshRecovery::ReimportCodex
            } else {
                SerializedRefreshRecovery::SignInAgain
            };
            return Self::refresh_serialized_rotating(
                inner,
                descriptor,
                bundle,
                expected_fence,
                registration,
                recovery,
            )
            .await;
        }
        let Some(refresh_token) = bundle.refresh_token() else {
            let _ =
                Self::mark_expired_if_current(inner, descriptor, bundle, expected_fence).await?;
            return Err(rotation_error(
                descriptor,
                haider_accounts::RotationTrigger::AuthExpired,
                false,
                "OAuth refresh token is unavailable",
            ));
        };
        if bundle
            .refresh_expires_at_unix_ms
            .is_some_and(|expires_at| now_ms().is_none_or(|now| now >= expires_at))
        {
            let _ =
                Self::mark_expired_if_current(inner, descriptor, bundle, expected_fence).await?;
            return Err(rotation_error(
                descriptor,
                haider_accounts::RotationTrigger::AuthExpired,
                false,
                "OAuth refresh token has expired",
            ));
        }
        let (begun, begin_result) = oneshot::channel();
        inner
            .status_commands
            .send(crate::accounts::AccountCommand::BeginOAuthRefresh {
                descriptor: descriptor.clone(),
                expected: refresh_fence(bundle, expected_fence),
                completed: begun,
            })
            .await
            .map_err(|_| {
                HaiderError::new(
                    ErrorCode::ProviderError,
                    "OAuth account actor is unavailable before refresh",
                    false,
                )
            })?;
        match begin_result.await {
            Ok(Ok(true)) => {}
            Ok(Ok(false)) => return Err(stale_refresh()),
            Ok(Err(error)) => return Err(error),
            Err(_) => {
                return Err(HaiderError::new(
                    ErrorCode::ProviderError,
                    "OAuth refresh preparation was lost",
                    false,
                ));
            }
        }
        let response = inner
            .refresh_exchange
            .exchange(&inner.client, &registration, refresh_token, None)
            .await;
        let refreshed = match response {
            Ok(bytes) => refresh_bundle_from_response(&registration, &bytes, bundle),
            Err(error) if error.retryable => return Err(oauth_error(error)),
            Err(error) => {
                let _ = Self::mark_expired_if_current(inner, descriptor, bundle, expected_fence)
                    .await?;
                let trigger = if error.code == "invalid_grant" {
                    haider_accounts::RotationTrigger::AuthExpired
                } else {
                    haider_accounts::RotationTrigger::RefreshFailed
                };
                return Err(rotation_error(
                    descriptor,
                    trigger,
                    false,
                    "OAuth refresh permanently failed",
                ));
            }
        };
        let refreshed = match refreshed {
            Ok(bundle) => bundle,
            Err(_) => {
                let _ = Self::mark_expired_if_current(inner, descriptor, bundle, expected_fence)
                    .await?;
                return Err(rotation_error(
                    descriptor,
                    haider_accounts::RotationTrigger::RefreshFailed,
                    false,
                    "OAuth refresh response failed validation",
                ));
            }
        };
        if fence.load(Ordering::Acquire) != expected_fence {
            return Err(stale_refresh());
        }
        let encoded = refreshed.encode()?;
        let (completed, result) = oneshot::channel();
        inner
            .status_commands
            .send(crate::accounts::AccountCommand::ApplyOAuthRefresh {
                descriptor: descriptor.clone(),
                expected: refresh_fence(bundle, expected_fence),
                encoded_bundle: encoded,
                completed,
            })
            .await
            .map_err(|_| {
                HaiderError::new(
                    ErrorCode::ProviderError,
                    "OAuth account actor is unavailable after refresh",
                    false,
                )
            })?;
        match result.await {
            Ok(Ok(())) => {}
            Ok(Err(crate::accounts::RefreshApplyError::Stale)) => {
                return Err(stale_refresh());
            }
            Ok(Err(crate::accounts::RefreshApplyError::Persist)) => {
                // A rotating server may already have invalidated the old
                // refresh token. Never release the new access token or retry
                // the old one.
                Self::invalidate_inner(inner, &descriptor.alias);
                return Err(rotation_error(
                    descriptor,
                    haider_accounts::RotationTrigger::RefreshFailed,
                    false,
                    "OAuth refresh could not be durably persisted; sign in again",
                ));
            }
            Err(_) => {
                return Err(HaiderError::new(
                    ErrorCode::ProviderError,
                    "OAuth refresh completion was lost",
                    false,
                ));
            }
        }
        if fence.load(Ordering::Acquire) != expected_fence {
            return Err(stale_refresh());
        }
        // MUTATION CHECK: returning before the successful `vault.put` above
        // is killed by refresh_vault_failure_never_returns_rotated_access.
        Ok(refreshed.access_token_handle())
    }

    async fn refresh_serialized_rotating(
        inner: &Arc<BrokerInner>,
        descriptor: &CredentialDescriptor,
        observed: &OAuthTokenBundleV1,
        expected_fence: u64,
        registration: Arc<OAuthProviderRegistration>,
        recovery: SerializedRefreshRecovery,
    ) -> Result<SecretHandle, HaiderError> {
        let lease = acquire_broker_refresh_lock(inner, &descriptor.alias).await?;
        if Self::fence_for_inner(inner, &descriptor.alias).load(Ordering::Acquire) != expected_fence
        {
            return Err(stale_refresh());
        }
        // The re-read is INSIDE the OS-lock critical section. A second daemon
        // that waited for generation N's rotation adopts durable N+1 without
        // ever replaying N's now-invalid refresh token.
        let stored = Self::resolve_vault_inner(inner, &descriptor.alias).await?;
        let current = OAuthTokenBundleV1::decode(stored.expose_secret())?;
        validate_bundle_against(&registration, descriptor, &current)?;
        let now = now_ms().ok_or_else(|| {
            HaiderError::new(ErrorCode::Internal, "system clock is unavailable", true)
        })?;
        if current
            .refresh_rejected_until_unix_ms
            .is_some_and(|until| now < until)
        {
            return Err(serialized_refresh_recovery_error(descriptor, recovery));
        }
        let current_refresh = current.refresh_token();
        let observed_refresh = observed.refresh_token();
        let refresh_changed = current.generation != observed.generation
            || match (current_refresh, observed_refresh) {
                (Some(current), Some(observed)) => !bool::from(current.ct_eq(observed)),
                (None, None) => false,
                _ => true,
            };
        if refresh_changed {
            if current.expires_at_unix_ms <= now {
                return Err(serialized_refresh_recovery_error(descriptor, recovery));
            }
            drop(lease);
            return Ok(current.access_token_handle());
        }
        let Some(refresh_token) = current_refresh else {
            return Err(serialized_refresh_recovery_error(descriptor, recovery));
        };
        if current
            .refresh_expires_at_unix_ms
            .is_some_and(|expires_at| now >= expires_at)
        {
            return Err(serialized_refresh_recovery_error(descriptor, recovery));
        }
        let device_id = if registration.auth_header_set == OAuthAuthHeaderSet::KimiMsh {
            Some(
                load_or_create_kimi_device_id(Arc::clone(&inner.vault))
                    .await
                    .map_err(oauth_error)?,
            )
        } else {
            None
        };
        // A rotating refresh request is an irreversible external mutation.
        // Persist permanent uncertainty BEFORE the request; every successful
        // or explicitly retryable response below replaces this marker while
        // still holding the physical-alias lease. A crash, cancellation, lost
        // actor completion, malformed success, or ambiguous transport error
        // therefore cannot replay the possibly spent token after restart.
        let uncertain = OAuthTokenBundleV1::decode(stored.expose_secret())?
            .with_refresh_rejected_until(u64::MAX);
        Self::apply_serialized_bundle(
            inner,
            descriptor,
            refresh_fence(&current, expected_fence),
            uncertain.encode()?,
            &descriptor.alias,
        )
        .await?;
        let mut attempts = 0_usize;
        let response = loop {
            attempts = attempts.saturating_add(1);
            let response = inner
                .refresh_exchange
                .exchange(
                    &inner.client,
                    &registration,
                    refresh_token,
                    device_id.as_ref(),
                )
                .await;
            let Some(delay) = response
                .as_ref()
                .err()
                .and_then(|error| kimi_refresh_retry_delay(error, attempts))
            else {
                break response;
            };
            tokio::time::sleep(delay).await;
            if inner.shutting_down.load(Ordering::Acquire) {
                return Err(broker_stopped());
            }
        };
        let bytes = match response {
            Ok(bytes) => bytes,
            Err(error) if error.retryable_status => {
                // Explicit 429/5xx is the provider-declared retryable class.
                // Restore the exact pre-request bundle before surfacing the
                // retry, so a later bounded attempt is possible.
                Self::apply_serialized_bundle(
                    inner,
                    descriptor,
                    refresh_fence(&current, expected_fence),
                    current.encode()?,
                    &descriptor.alias,
                )
                .await?;
                return Err(oauth_error(error));
            }
            Err(error) if error.retryable => {
                // A transport failure is ambiguous: the server may have
                // rotated before the response was lost. Keep permanent
                // uncertainty and require a new login.
                return Err(serialized_refresh_recovery_error(descriptor, recovery));
            }
            Err(_) => {
                // Re-read once after a terminal rejection. This covers a
                // winner that persisted between the caller's stale 401 and
                // our serialized refresh attempt. Only the unchanged exact
                // token is tombstoned.
                let latest = Self::resolve_vault_inner(inner, &descriptor.alias).await?;
                let latest = OAuthTokenBundleV1::decode(latest.expose_secret())?;
                validate_bundle_against(&registration, descriptor, &latest)?;
                let latest_changed = latest.generation != current.generation
                    || match (latest.refresh_token(), current.refresh_token()) {
                        (Some(latest), Some(current)) => !bool::from(latest.ct_eq(current)),
                        (None, None) => false,
                        _ => true,
                    };
                if latest_changed && latest.expires_at_unix_ms > now {
                    drop(lease);
                    return Ok(latest.access_token_handle());
                }
                let expected = refresh_fence(&current, expected_fence);
                let tombstoned = current.with_refresh_rejected_until(
                    now.saturating_add(duration_ms(KIMI_REFRESH_REJECTED_TTL)),
                );
                let encoded = tombstoned.encode()?;
                Self::apply_serialized_bundle(
                    inner,
                    descriptor,
                    expected,
                    encoded,
                    &descriptor.alias,
                )
                .await?;
                drop(lease);
                return Err(serialized_refresh_recovery_error(descriptor, recovery));
            }
        };
        let refreshed = refresh_bundle_from_response(&registration, &bytes, &current)
            .map_err(|_| serialized_refresh_recovery_error(descriptor, recovery))?;
        let expected = refresh_fence(&current, expected_fence);
        let access = refreshed.access_token_handle();
        let encoded = refreshed.encode()?;
        Self::apply_serialized_bundle(inner, descriptor, expected, encoded, &descriptor.alias)
            .await?;
        // Persist completed while the lease was held; access may escape only
        // after this point.
        drop(lease);
        Ok(access)
    }

    async fn apply_serialized_bundle(
        inner: &Arc<BrokerInner>,
        descriptor: &CredentialDescriptor,
        expected: crate::accounts::OAuthRefreshFence,
        encoded_bundle: Zeroizing<Vec<u8>>,
        alias: &CredentialAlias,
    ) -> Result<(), HaiderError> {
        let (completed, result) = oneshot::channel();
        inner
            .status_commands
            .send(crate::accounts::AccountCommand::ApplyOAuthRefresh {
                descriptor: descriptor.clone(),
                expected,
                encoded_bundle,
                completed,
            })
            .await
            .map_err(|_| {
                HaiderError::new(
                    ErrorCode::ProviderError,
                    "OAuth account actor is unavailable after refresh",
                    false,
                )
            })?;
        match result.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(crate::accounts::RefreshApplyError::Stale)) => Err(stale_refresh()),
            Ok(Err(crate::accounts::RefreshApplyError::Persist)) => {
                Self::invalidate_inner(inner, alias);
                Err(rotation_error(
                    descriptor,
                    haider_accounts::RotationTrigger::RefreshFailed,
                    false,
                    "OAuth refresh could not be durably persisted; sign in again",
                ))
            }
            Err(_) => Err(HaiderError::new(
                ErrorCode::ProviderError,
                "OAuth refresh completion was lost",
                false,
            )),
        }
    }

    async fn resolve_vault_inner(
        inner: &Arc<BrokerInner>,
        alias: &CredentialAlias,
    ) -> Result<SecretHandle, HaiderError> {
        let vault = Arc::clone(&inner.vault);
        let alias = alias.clone();
        tokio::task::spawn_blocking(move || vault.resolve(&alias))
            .await
            .map_err(|_| HaiderError::new(ErrorCode::ProviderError, "vault worker failed", true))?
    }

    async fn resolve_vault(&self, alias: &CredentialAlias) -> Result<SecretHandle, HaiderError> {
        Self::resolve_vault_inner(&self.inner, alias).await
    }

    fn validate_bundle(
        &self,
        descriptor: &CredentialDescriptor,
        bundle: &OAuthTokenBundleV1,
    ) -> Result<(), HaiderError> {
        let Some(registration) = self.inner.catalog.registration(&descriptor.provider) else {
            return Err(HaiderError::new(
                ErrorCode::Unauthorized,
                "OAuth registration is unavailable; sign in again",
                false,
            ));
        };
        validate_bundle_against(&registration, descriptor, bundle)
    }

    fn fence_for(&self, alias: &CredentialAlias) -> Arc<AtomicU64> {
        Self::fence_for_inner(&self.inner, alias)
    }

    fn snapshot_allows_oauth(inner: &BrokerInner, expected: &CredentialDescriptor) -> bool {
        Self::snapshot_oauth_state(inner, expected) == SnapshotOAuthState::Current
    }

    fn snapshot_oauth_state(
        inner: &BrokerInner,
        expected: &CredentialDescriptor,
    ) -> SnapshotOAuthState {
        inner
            .snapshot
            .lock()
            .map_or(SnapshotOAuthState::Replaced, |descriptors| {
                descriptors
                    .iter()
                    .find(|current| {
                        current.alias == expected.alias
                            && current.provider == expected.provider
                            && current.base_url == expected.base_url
                            && current.auth_method == expected.auth_method
                            && current.identity == expected.identity
                    })
                    .map_or(SnapshotOAuthState::Replaced, |current| {
                        if matches!(
                            &current.status,
                            CredentialStatus::Expired | CredentialStatus::NeedsAttention { .. }
                        ) {
                            SnapshotOAuthState::Expired
                        } else {
                            SnapshotOAuthState::Current
                        }
                    })
            })
    }

    fn current_oauth_descriptor(
        inner: &BrokerInner,
        expected: &CredentialDescriptor,
    ) -> Option<CredentialDescriptor> {
        inner.snapshot.lock().ok()?.iter().find_map(|current| {
            (current.alias == expected.alias
                && current.provider == expected.provider
                && current.base_url == expected.base_url
                && current.auth_method == expected.auth_method
                && !matches!(
                    &current.status,
                    CredentialStatus::Expired | CredentialStatus::NeedsAttention { .. }
                ))
            .then(|| current.clone())
        })
    }

    fn active_flight(&self, descriptor: &CredentialDescriptor) -> Option<Arc<RefreshFlight>> {
        self.inner
            .flights
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|(key, _)| {
                key.alias == descriptor.alias.as_str() && key.provider == descriptor.provider
            })
            .map(|(_, flight)| Arc::clone(flight))
    }

    fn fence_for_inner(inner: &BrokerInner, alias: &CredentialAlias) -> Arc<AtomicU64> {
        inner.fences.fence_for(alias)
    }

    /// Removal/replacement fence used by W5c and race tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn invalidate(&self, alias: &CredentialAlias) {
        Self::invalidate_inner(&self.inner, alias);
    }

    fn invalidate_inner(inner: &BrokerInner, alias: &CredentialAlias) {
        inner.fences.invalidate(alias);
    }

    pub(crate) async fn shutdown(&self) -> bool {
        self.inner.shutting_down.store(true, Ordering::Release);
        let graceful = self.tasks.join_all().await;
        if !graceful {
            poison_refresh_flights(&self.inner);
        }
        graceful
    }

    pub(crate) async fn abort_and_join(&self) {
        self.inner.shutting_down.store(true, Ordering::Release);
        self.tasks.abort_and_join().await;
        poison_refresh_flights(&self.inner);
    }

    #[cfg(test)]
    fn panic_next_refresh_worker(&self) {
        self.inner
            .panic_refresh_worker
            .store(true, Ordering::Release);
    }

    async fn mark_expired_if_current(
        inner: &BrokerInner,
        descriptor: &CredentialDescriptor,
        bundle: &OAuthTokenBundleV1,
        expected_fence: u64,
    ) -> Result<bool, HaiderError> {
        if Self::fence_for_inner(inner, &descriptor.alias).load(Ordering::Acquire) != expected_fence
        {
            return Ok(false);
        }
        let (completed, result) = oneshot::channel();
        inner
            .status_commands
            .send(crate::accounts::AccountCommand::ExpireOAuthRefresh {
                descriptor: descriptor.clone(),
                expected: refresh_fence(bundle, expected_fence),
                completed,
            })
            .await
            .map_err(|_| {
                HaiderError::new(
                    ErrorCode::ProviderError,
                    "OAuth account status actor is unavailable",
                    false,
                )
            })?;
        result.await.map_err(|_| {
            HaiderError::new(
                ErrorCode::ProviderError,
                "OAuth account status completion was lost",
                false,
            )
        })?
    }
}

fn registration_is_serialized_rotating(catalog: &OAuthProviderCatalog, provider: &str) -> bool {
    catalog.registration(provider).is_some_and(|registration| {
        registration.refresh_policy == OAuthRefreshPolicy::SerializedRotating
    })
}

fn kimi_refresh_retry_delay(error: &OAuthPublicError, attempts: usize) -> Option<Duration> {
    (error.retryable_status && attempts < KIMI_REFRESH_MAX_ATTEMPTS).then(|| {
        let exponent = u32::try_from(attempts.saturating_sub(1)).unwrap_or(u32::MAX);
        KIMI_REFRESH_BACKOFF_INITIAL.saturating_mul(2_u32.saturating_pow(exponent))
    })
}

fn validate_bundle_against(
    registration: &OAuthProviderRegistration,
    descriptor: &CredentialDescriptor,
    bundle: &OAuthTokenBundleV1,
) -> Result<(), HaiderError> {
    if bundle.provider_id != descriptor.provider
        || bundle.issuer != registration.issuer
        || bundle.audience != registration.audience
        || bundle.resource != registration.resource
        || !bundle.token_type.eq_ignore_ascii_case("bearer")
        || !registration
            .scopes
            .iter()
            .filter(|scope| registration.validation_required(scope))
            .all(|scope| bundle.granted_scopes.contains(scope))
    {
        return Err(HaiderError::new(
            ErrorCode::Unauthorized,
            "stored OAuth token bundle failed provider validation",
            false,
        ));
    }
    Ok(())
}

async fn acquire_refresh_lock(
    vault: Arc<dyn Vault>,
    alias: &CredentialAlias,
) -> Result<VaultRefreshLock, HaiderError> {
    loop {
        let vault_for_lock = Arc::clone(&vault);
        let alias_for_lock = alias.clone();
        let lease =
            tokio::task::spawn_blocking(move || vault_for_lock.try_refresh_lock(&alias_for_lock))
                .await
                .map_err(|_| {
                    HaiderError::new(
                        ErrorCode::ProviderError,
                        "vault refresh-lock worker failed",
                        true,
                    )
                })??;
        if let Some(lease) = lease {
            return Ok(lease);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn acquire_broker_refresh_lock(
    inner: &Arc<BrokerInner>,
    alias: &CredentialAlias,
) -> Result<VaultRefreshLock, HaiderError> {
    loop {
        if inner.shutting_down.load(Ordering::Acquire) {
            return Err(broker_stopped());
        }
        let vault = Arc::clone(&inner.vault);
        let alias_for_lock = alias.clone();
        let lease = tokio::task::spawn_blocking(move || vault.try_refresh_lock(&alias_for_lock))
            .await
            .map_err(|_| {
                HaiderError::new(
                    ErrorCode::ProviderError,
                    "vault refresh-lock worker failed",
                    true,
                )
            })??;
        if let Some(lease) = lease {
            return Ok(lease);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn refresh_fence(
    bundle: &OAuthTokenBundleV1,
    fence_epoch: u64,
) -> crate::accounts::OAuthRefreshFence {
    crate::accounts::OAuthRefreshFence {
        fence_epoch,
        generation: bundle.generation,
        issuer: bundle.issuer.clone(),
        audience: bundle.audience.clone(),
        resource: bundle.resource.clone(),
        subject_hash: bundle.identity.subject_hash.clone(),
    }
}

fn broker_stopped() -> HaiderError {
    HaiderError::new(
        ErrorCode::ProviderError,
        "OAuth refresh broker is shutting down",
        true,
    )
}

fn refresh_worker_failed() -> HaiderError {
    HaiderError::new(
        ErrorCode::ProviderError,
        "OAuth refresh worker failed before durable completion",
        false,
    )
}

#[derive(Deserialize)]
struct RefreshTokenResponse {
    access_token: SecretJson,
    #[serde(default)]
    refresh_token: Option<SecretJson>,
    token_type: String,
    expires_in: u64,
    #[serde(default)]
    refresh_expires_in: Option<u64>,
    scope: String,
    #[serde(default, alias = "iss")]
    issuer: Option<String>,
    #[serde(default)]
    audience: Option<String>,
    #[serde(default)]
    resource: Option<String>,
}

async fn exchange_refresh_token(
    client: &reqwest::Client,
    registration: &OAuthProviderRegistration,
    refresh_token: &[u8],
    device_id: Option<&SecretHandle>,
) -> Result<Zeroizing<Vec<u8>>, OAuthPublicError> {
    let body = refresh_token_request_body(registration, refresh_token)?;
    let content_type = body.content_type();
    let request = client
        .post(registration.token_endpoint.clone())
        .header(reqwest::header::CONTENT_TYPE, content_type)
        .header(reqwest::header::CONNECTION, "close")
        .body(reqwest::Body::from(bytes::Bytes::from_owner(body)));
    let response = apply_oauth_auth_headers(request, registration, device_id)?
        .send()
        .await
        .map_err(|_| OAuthPublicError::new("token_endpoint_unavailable", true))?;
    if response.status().is_redirection() {
        return Err(OAuthPublicError::new("token_redirect_rejected", false));
    }
    let status = response.status();
    let bytes = bounded_response(response).await?;
    if status.is_success() {
        Ok(bytes)
    } else {
        Err(classify_token_error(status.as_u16(), &bytes))
    }
}

fn refresh_token_request_body(
    registration: &OAuthProviderRegistration,
    refresh_token: &[u8],
) -> Result<SecretTokenBody, OAuthPublicError> {
    let refresh_token = std::str::from_utf8(refresh_token)
        .map_err(|_| OAuthPublicError::new("invalid_refresh_token", false))?;
    let request = RefreshTokenRequest {
        grant_type: "refresh_token",
        refresh_token,
        client_id: &registration.client_id,
        audience: registration
            .refresh_includes_binding
            .then_some(registration.audience.as_str()),
        resource: registration
            .refresh_includes_binding
            .then_some(registration.resource.as_deref())
            .flatten(),
    };
    match registration.refresh_encoding {
        OAuthTokenRequestEncoding::Form => {
            let mut encoded = url::form_urlencoded::Serializer::new(SecretFormBody::empty());
            encoded
                .append_pair("grant_type", request.grant_type)
                .append_pair("refresh_token", request.refresh_token)
                .append_pair("client_id", request.client_id);
            if let Some(audience) = request.audience {
                encoded.append_pair("audience", audience);
            }
            if let Some(resource) = request.resource {
                encoded.append_pair("resource", resource);
            }
            Ok(SecretTokenBody::Form(encoded.finish()))
        }
        OAuthTokenRequestEncoding::Json => Ok(SecretTokenBody::Json(encode_secret_json(&request)?)),
    }
}

fn refresh_bundle_from_response(
    registration: &OAuthProviderRegistration,
    bytes: &[u8],
    prior: &OAuthTokenBundleV1,
) -> Result<OAuthTokenBundleV1, OAuthPublicError> {
    let response = serde_json::from_slice::<RefreshTokenResponse>(bytes)
        .map_err(|_| OAuthPublicError::new("malformed_token_response", false))?;
    if prior.issuer != registration.issuer
        || prior.audience != registration.audience
        || prior.resource != registration.resource
        || response
            .issuer
            .as_deref()
            .is_some_and(|issuer| issuer != prior.issuer)
        || response
            .audience
            .as_deref()
            .is_some_and(|audience| audience != prior.audience)
        || response
            .resource
            .as_deref()
            .is_some_and(|resource| Some(resource) != prior.resource.as_deref())
        || !response.token_type.eq_ignore_ascii_case("bearer")
        || response.expires_in == 0
        || response.expires_in > MAX_TOKEN_LIFETIME_SECS
        || response
            .refresh_expires_in
            .is_some_and(|expiry| expiry == 0 || expiry > MAX_TOKEN_LIFETIME_SECS)
    {
        return Err(OAuthPublicError::new("invalid_token_response", false));
    }
    let scopes = response
        .scope
        .split_ascii_whitespace()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    if !registration
        .scopes
        .iter()
        .filter(|scope| registration.validation_required(scope))
        .all(|scope| scopes.contains(scope))
    {
        return Err(OAuthPublicError::new("scope_mismatch", false));
    }
    let now = now_ms().ok_or_else(|| OAuthPublicError::new("clock_unavailable", true))?;
    let refresh_token = match response.refresh_token {
        Some(token) => Some(token.0),
        None if registration.retain_refresh_on_omission => prior
            .refresh_token()
            .map(|token| Zeroizing::new(token.to_vec())),
        None => return Err(OAuthPublicError::new("missing_refresh_token", false)),
    };
    let refresh_expires_at = match response.refresh_expires_in {
        Some(seconds) => now.checked_add(seconds.saturating_mul(1000)),
        None => prior.refresh_expires_at_unix_ms,
    };
    let mut bundle = OAuthTokenBundleV1::new(
        registration.provider_id.clone(),
        registration.issuer.clone(),
        prior.audience.clone(),
        prior.resource.clone(),
        response.token_type,
        response.access_token.0,
        refresh_token,
        now.checked_add(response.expires_in.saturating_mul(1000))
            .ok_or_else(|| OAuthPublicError::new("invalid_token_response", false))?,
        refresh_expires_at,
        scopes.into_iter().collect(),
        prior.identity.clone(),
        prior
            .generation
            .checked_add(1)
            .ok_or_else(|| OAuthPublicError::new("invalid_token_response", false))?,
    )
    .map_err(|_| OAuthPublicError::new("invalid_token_response", false))?;
    if let Some(fingerprint) = prior.import_source_access_fingerprint() {
        bundle = bundle.with_import_source_access_fingerprint(fingerprint);
    }
    if let Some(identity) = prior.account_identity.clone() {
        bundle = bundle.with_account_identity(identity);
    }
    if let Some(id_token) = prior.id_token() {
        bundle = bundle.with_id_token(Zeroizing::new(id_token.to_vec()));
    }
    Ok(
        if registration.refresh_policy == OAuthRefreshPolicy::SerializedRotating {
            bundle.with_refresh_after(kimi_refresh_after(now, response.expires_in))
        } else {
            bundle
        },
    )
}

fn rotation_error(
    descriptor: &CredentialDescriptor,
    trigger: haider_accounts::RotationTrigger,
    retryable: bool,
    message: &str,
) -> HaiderError {
    let mut error = HaiderError::new(ErrorCode::Unauthorized, message, retryable);
    error.details = Some(serde_json::json!({
        "alias": descriptor.alias.as_str(),
        "rotation_trigger": match trigger {
            haider_accounts::RotationTrigger::RateLimit { .. } => "rate_limit",
            haider_accounts::RotationTrigger::AuthExpired => "auth_expired",
            haider_accounts::RotationTrigger::RefreshFailed => "refresh_failed",
        },
        "rotation_cause": match trigger.cause() {
            haider_protocol::credential::RotationCause::RateLimit => "rate_limit",
            haider_protocol::credential::RotationCause::Error => "error",
            haider_protocol::credential::RotationCause::Manual => "manual",
        }
    }));
    error
}

fn kimi_relogin_required(descriptor: &CredentialDescriptor) -> HaiderError {
    let mut error = rotation_error(
        descriptor,
        haider_accounts::RotationTrigger::AuthExpired,
        false,
        "Kimi OAuth refresh was rejected; sign in again",
    );
    if let Some(details) = error
        .details
        .as_mut()
        .and_then(serde_json::Value::as_object_mut)
    {
        details.insert(
            "kind".to_owned(),
            serde_json::Value::String("oauth_relogin_required".to_owned()),
        );
        details.insert("relogin_required".to_owned(), serde_json::Value::Bool(true));
    }
    error.presentation = Some(ErrorPresentation::new(
        "oauth-expired",
        "Sign-in expired",
        "The OAuth credential could not be refreshed.",
        ErrorScope::Account,
        [ErrorAction::Relogin, ErrorAction::SwitchAccount],
    ));
    error
}

fn serialized_refresh_recovery_error(
    descriptor: &CredentialDescriptor,
    recovery: SerializedRefreshRecovery,
) -> HaiderError {
    match recovery {
        SerializedRefreshRecovery::SignInAgain => kimi_relogin_required(descriptor),
        SerializedRefreshRecovery::ReimportCodex => {
            imported_credential_expired(descriptor, "codex")
        }
    }
}

fn imported_credential_expired(descriptor: &CredentialDescriptor, source: &str) -> HaiderError {
    let mut error = rotation_error(
        descriptor,
        haider_accounts::RotationTrigger::AuthExpired,
        false,
        &format!(
            "credential expired — re-run `haider account import {source} --confirm` or sign in again"
        ),
    );
    if let Some(details) = error
        .details
        .as_mut()
        .and_then(serde_json::Value::as_object_mut)
    {
        details.insert(
            "kind".to_owned(),
            serde_json::Value::String("oauth_relogin_required".to_owned()),
        );
        details.insert(
            "reimport_required".to_owned(),
            serde_json::Value::Bool(true),
        );
        details.insert(
            "import_source".to_owned(),
            serde_json::Value::String(source.to_owned()),
        );
    }
    error.presentation = Some(ErrorPresentation::new(
        "reimport-required",
        "Imported sign-in expired",
        "Re-import the local credential or sign in again.",
        ErrorScope::Account,
        [
            ErrorAction::Reimport,
            ErrorAction::Relogin,
            ErrorAction::SwitchAccount,
        ],
    ));
    error
}

fn imported_refresh_error(
    error: HaiderError,
    descriptor: &CredentialDescriptor,
    source: &str,
) -> HaiderError {
    if error.retryable
        || error
            .details
            .as_ref()
            .and_then(|details| details.get("kind"))
            .and_then(serde_json::Value::as_str)
            == Some("oauth_relogin_required")
    {
        error
    } else {
        imported_credential_expired(descriptor, source)
    }
}

fn imported_credential_expired_for_provider(
    descriptor: &CredentialDescriptor,
    provider: &str,
) -> HaiderError {
    match provider {
        haider_provider::OPENAI_OAUTH_PROVIDER_NAME => {
            imported_credential_expired(descriptor, "codex")
        }
        haider_provider::ANTHROPIC_OAUTH_PROVIDER_NAME => {
            imported_credential_expired(descriptor, "claude-code")
        }
        _ => expired_or_replaced(descriptor),
    }
}

fn expired_or_replaced(descriptor: &CredentialDescriptor) -> HaiderError {
    rotation_error(
        descriptor,
        haider_accounts::RotationTrigger::AuthExpired,
        false,
        "OAuth credential is expired or was replaced",
    )
}

fn stale_refresh() -> HaiderError {
    HaiderError::new(
        ErrorCode::Unauthorized,
        "OAuth refresh completion was fenced by account removal or replacement",
        false,
    )
}

pub(crate) fn oauth_error(error: OAuthPublicError) -> HaiderError {
    let code = if error.code == "invalid_grant" {
        ErrorCode::Unauthorized
    } else {
        ErrorCode::ProviderError
    };
    HaiderError::new(
        code,
        format!("OAuth failure: {}", error.code),
        error.retryable,
    )
}

#[cfg(test)]
#[path = "oauth_tests.rs"]
mod tests;
