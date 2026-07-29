//! Daemon-owned OAuth authorization-code/PKCE and credential refresh.
//!
//! Live provider grants are a release-owned allowlist, not user
//! configuration. The shipped sanctioned table is intentionally empty; tests
//! inject a loopback-only fake registration through [`AccountsDependencies`].

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use haider_accounts::{OAuthIdentityV1, OAuthTokenBundleV1};
use haider_accounts::{SecretHandle, Vault};
use haider_protocol::credential::{AuthMethod, CredentialDescriptor, CredentialStatus};
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::ids::CredentialAlias;
use haider_rpc::{
    ERROR_CODE_BUSY, OAuthAuthorizationWire, OAuthAvailabilityWire, OAuthFlowId,
    OAuthFlowStatusWire, OAuthReadyRefWire, RequestId, ResponseBody, WireFrame,
};
use reqwest::redirect::Policy;
use serde::de::Visitor;
use serde::{Deserialize, Deserializer};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio::time::Instant;
use url::Url;
use zeroize::Zeroizing;

use crate::accounts::SECRET_TTL;
use crate::session_hub::FrameSink;

const CALLBACK_RESPONSE_LIMIT: usize = 8 * 1024;
const TOKEN_RESPONSE_LIMIT: usize = 256 * 1024;
const MIN_RANDOM_BYTES: usize = 32;
const MAX_TOKEN_LIFETIME_SECS: u64 = 366 * 24 * 60 * 60;
const CALLBACK_READ_TIMEOUT: Duration = Duration::from_secs(2);
const TOKEN_TIMEOUT: Duration = Duration::from_secs(15);

/// Release-owned provider metadata shape.
///
/// OWNER FILL POINT: this table is populated only after Haider receives a
/// sanctioned public-native client registration and documented inference
/// scopes. A reference CLI registration never belongs here.
#[derive(Debug, Clone, Copy)]
pub struct SanctionedOAuthRegistration {
    pub provider_id: &'static str,
    pub issuer: &'static str,
    pub authorization_endpoint: &'static str,
    pub token_endpoint: &'static str,
    pub client_id: &'static str,
    pub scopes: &'static [&'static str],
    pub audience: &'static str,
    pub redirect_policy: OAuthRedirectPolicy,
    pub retain_refresh_on_omission: bool,
    pub identity_verifier_factory: fn() -> Arc<dyn OAuthIdentityVerifier>,
}

/// The only callback policy supported by the generic engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthRedirectPolicy {
    /// Bind a fresh numeric `127.0.0.1:0` listener and use a random path.
    EphemeralIpv4Loopback,
}

/// Intentionally empty at ship. Do not add copied OpenAI Codex metadata,
/// guessed ChatGPT scopes, or third-party Claude Max login here.
pub const SANCTIONED_PROVIDER_REGISTRATIONS: &[SanctionedOAuthRegistration] = &[];

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
pub trait OAuthIdentityVerifier: Send + Sync {
    fn verify(
        &self,
        id_token: &[u8],
        expected: OAuthIdentityExpectation<'_>,
    ) -> Result<OAuthIdentityV1, OAuthPublicError>;
}

/// A sanitized OAuth failure. `Debug` intentionally omits endpoint bodies.
#[derive(Clone, PartialEq, Eq)]
pub struct OAuthPublicError {
    pub code: &'static str,
    pub retryable: bool,
}

impl OAuthPublicError {
    pub const fn new(code: &'static str, retryable: bool) -> Self {
        Self { code, retryable }
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
    scopes: BTreeSet<String>,
    audience: String,
    redirect_policy: OAuthRedirectPolicy,
    retain_refresh_on_omission: bool,
    identity_verifier: Arc<dyn OAuthIdentityVerifier>,
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
            .field("redirect_policy", &self.redirect_policy)
            .field(
                "retain_refresh_on_omission",
                &self.retain_refresh_on_omission,
            )
            .field("identity_verifier", &"[VERIFIER]")
            .finish()
    }
}

impl OAuthProviderRegistration {
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
        retain_refresh_on_omission: bool,
        identity_verifier: Arc<dyn OAuthIdentityVerifier>,
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
        let scopes = scopes.into_iter().collect::<BTreeSet<_>>();
        if provider_id.trim().is_empty()
            || issuer.trim().is_empty()
            || client_id.trim().is_empty()
            || audience.trim().is_empty()
            || scopes.is_empty()
            || scopes.iter().any(|scope| scope.trim().is_empty())
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
            redirect_policy: OAuthRedirectPolicy::EphemeralIpv4Loopback,
            retain_refresh_on_omission,
            identity_verifier,
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
                OAuthProviderRegistration::new(
                    metadata.provider_id,
                    metadata.issuer,
                    metadata.authorization_endpoint,
                    metadata.token_endpoint,
                    metadata.client_id,
                    metadata.scopes.iter().map(|scope| (*scope).to_owned()),
                    metadata.audience,
                    metadata.retain_refresh_on_omission,
                    (metadata.identity_verifier_factory)(),
                )
                .ok()
                .map(|registration| (registration.provider_id.clone(), Arc::new(registration)))
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

fn is_numeric_loopback_http(url: &Url) -> bool {
    url.scheme() == "http" && matches!(url.host(), Some(url::Host::Ipv4(ip)) if ip.is_loopback())
}

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
            flow_ttl: SECRET_TTL,
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
    next_flow: AtomicU64,
    client: reqwest::Client,
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
}

impl OAuthCoordinator {
    pub(crate) fn new(
        instance_id: String,
        catalog: OAuthProviderCatalog,
        config: OAuthCoordinatorConfig,
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
        let inner = Arc::new(CoordinatorInner {
            instance_id,
            catalog,
            config,
            flows: Mutex::new(HashMap::new()),
            active_connections: Mutex::new(HashSet::new()),
            shutting_down: AtomicBool::new(false),
            next_flow: AtomicU64::new(0),
            client,
        });
        let (starts, receiver) = mpsc::channel(config.max_flows);
        tokio::spawn(run_start_worker(Arc::downgrade(&inner), receiver));
        Ok(Self { inner, starts })
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

    pub(crate) fn shutdown(&self) {
        self.inner.shutting_down.store(true, Ordering::Release);
        if let Ok(mut active) = self.inner.active_connections.lock() {
            active.clear();
        }
        if let Ok(mut flows) = self.inner.flows.lock() {
            for (_, flow) in flows.drain() {
                flow.cancel.send_replace(true);
            }
        }
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

async fn run_start_worker(inner: Weak<CoordinatorInner>, mut receiver: mpsc::Receiver<StartJob>) {
    while let Some(job) = receiver.recv().await {
        let Some(inner) = inner.upgrade() else {
            break;
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
    let callback_path = Zeroizing::new(format!("/oauth/callback/{}", callback_segment.as_str()));
    let redirect_uri = Zeroizing::new(format!(
        "http://127.0.0.1:{}{}",
        bound.port(),
        callback_path.as_str()
    ));
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier_b64.as_bytes()));
    let mut authorization = url::form_urlencoded::Serializer::new(SecretFormBody::new(format!(
        "{}?",
        registration.authorization_endpoint
    )));
    authorization
        .append_pair("response_type", "code")
        .append_pair("client_id", &registration.client_id)
        .append_pair("redirect_uri", redirect_uri.as_str())
        .append_pair(
            "scope",
            &registration
                .scopes
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(" "),
        )
        .append_pair("state", state_b64.as_str())
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("nonce", nonce_b64.as_str())
        .append_pair("audience", &registration.audience);
    // `finish` moves the one state-bearing allocation directly into the
    // zeroizing wire wrapper. No second ordinary URL retains the query.
    let authorization_url =
        OAuthAuthorizationWire::from_zeroizing(authorization.finish().into_zeroizing());
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
        },
    );
    tokio::spawn(run_callback_flow(
        inner,
        flow_id,
        registration,
        listener,
        callback_path,
        bound.port(),
        state_b64,
        verifier_b64,
        nonce_b64,
        redirect_uri,
        cancel_rx,
    ));
}

#[allow(clippy::too_many_arguments)]
async fn run_callback_flow(
    inner: Arc<CoordinatorInner>,
    flow_id: OAuthFlowId,
    registration: Arc<OAuthProviderRegistration>,
    listener: TcpListener,
    callback_path: Zeroizing<String>,
    port: u16,
    expected_state: Zeroizing<String>,
    verifier: Zeroizing<String>,
    nonce: Zeroizing<String>,
    redirect_uri: Zeroizing<String>,
    mut cancel: watch::Receiver<bool>,
) {
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
                let callback_result = {
                    let callback = read_callback(
                        &mut stream,
                        callback_path.as_str(),
                        port,
                        expected_state.as_bytes(),
                    );
                    tokio::pin!(callback);
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
                            continue;
                        }
                        result = &mut callback => result,
                    }
                };
                match callback_result {
                    CallbackResult::Invalid => {
                        invalid = invalid.saturating_add(1);
                        let _ = send_callback_page(&mut stream, 400, INVALID_HTML).await;
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

enum CallbackResult {
    Invalid,
    Denied(&'static str),
    Code(Zeroizing<Vec<u8>>),
}

async fn read_callback(
    stream: &mut TcpStream,
    expected_path: &str,
    port: u16,
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
        return CallbackResult::Invalid;
    }
    parse_callback(&request, expected_path, port, expected_state)
}

fn parse_callback(
    request: &[u8],
    expected_path: &str,
    port: u16,
    expected_state: &[u8],
) -> CallbackResult {
    let Ok(text) = std::str::from_utf8(request) else {
        return CallbackResult::Invalid;
    };
    if !text.is_ascii() {
        return CallbackResult::Invalid;
    }
    let Some(header_end) = text.find("\r\n\r\n") else {
        return CallbackResult::Invalid;
    };
    if !text[header_end + 4..].is_empty() {
        return CallbackResult::Invalid;
    }
    let mut lines = text[..header_end].split("\r\n");
    let Some(request_line) = lines.next() else {
        return CallbackResult::Invalid;
    };
    let mut parts = request_line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return CallbackResult::Invalid;
    };
    if method != "GET" || version != "HTTP/1.1" || !target.starts_with('/') || target.contains('#')
    {
        return CallbackResult::Invalid;
    }
    let mut host = None;
    let mut content_length = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            return CallbackResult::Invalid;
        };
        if name.eq_ignore_ascii_case("host") {
            if host.replace(value.trim()).is_some() {
                return CallbackResult::Invalid;
            }
        } else if name.eq_ignore_ascii_case("content-length") {
            if content_length.replace(value.trim()).is_some() {
                return CallbackResult::Invalid;
            }
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            return CallbackResult::Invalid;
        }
    }
    if content_length.is_some_and(|length| length != "0") {
        return CallbackResult::Invalid;
    }
    let expected_host = format!("127.0.0.1:{port}");
    if host != Some(expected_host.as_str()) {
        return CallbackResult::Invalid;
    }
    let Some((path, query)) = target.split_once('?') else {
        return CallbackResult::Invalid;
    };
    if path != expected_path || query.is_empty() {
        return CallbackResult::Invalid;
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
        return CallbackResult::Invalid;
    }
    if let Some(code) = codes.pop() {
        if code.is_empty() || code.len() > 4096 {
            return CallbackResult::Invalid;
        }
        CallbackResult::Code(code)
    } else {
        match errors.pop().as_ref().map(|error| error.as_slice()) {
            Some(b"access_denied") => CallbackResult::Denied("access_denied"),
            Some(_) => CallbackResult::Denied("authorization_denied"),
            None => CallbackResult::Invalid,
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

async fn exchange_authorization_code(
    client: &reqwest::Client,
    registration: &OAuthProviderRegistration,
    code: &[u8],
    verifier: &[u8],
    nonce: &[u8],
    redirect_uri: &str,
) -> Result<OAuthTokenBundleV1, OAuthPublicError> {
    let code = std::str::from_utf8(code)
        .map_err(|_| OAuthPublicError::new("invalid_authorization_code", false))?;
    let verifier = std::str::from_utf8(verifier)
        .map_err(|_| OAuthPublicError::new("invalid_pkce_verifier", false))?;
    let body = {
        let mut encoded = url::form_urlencoded::Serializer::new(SecretFormBody::empty());
        encoded
            .append_pair("grant_type", "authorization_code")
            .append_pair("code", code)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("client_id", &registration.client_id)
            .append_pair("code_verifier", verifier);
        encoded.finish()
    };
    let response = client
        .post(registration.token_endpoint.clone())
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
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
    token_bundle_from_response(registration, &bytes, nonce, 1, None)
}

async fn bounded_response(
    mut response: reqwest::Response,
) -> Result<Zeroizing<Vec<u8>>, OAuthPublicError> {
    if response
        .content_length()
        .is_some_and(|length| length > TOKEN_RESPONSE_LIMIT as u64)
    {
        return Err(OAuthPublicError::new("token_response_oversized", false));
    }
    let mut bytes = Zeroizing::new(Vec::new());
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| OAuthPublicError::new("token_endpoint_unavailable", true))?
    {
        if bytes.len().saturating_add(chunk.len()) > TOKEN_RESPONSE_LIMIT {
            return Err(OAuthPublicError::new("token_response_oversized", false));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
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
    if kind
        .as_ref()
        .is_some_and(|kind| kind.0.as_slice() == b"invalid_grant")
    {
        OAuthPublicError::new("invalid_grant", false)
    } else if status >= 500 {
        OAuthPublicError::new("token_endpoint_unavailable", true)
    } else {
        OAuthPublicError::new("token_exchange_failed", false)
    }
}

struct SecretJson(Zeroizing<Vec<u8>>);

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
    id_token: SecretJson,
}

fn token_bundle_from_response(
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
    if !registration.scopes.is_subset(&scopes) {
        return Err(OAuthPublicError::new("scope_mismatch", false));
    }
    let identity = registration.identity_verifier.verify(
        response.id_token.0.as_slice(),
        OAuthIdentityExpectation {
            issuer: &registration.issuer,
            audience: &registration.audience,
            nonce,
        },
    )?;
    let now = now_ms().ok_or_else(|| OAuthPublicError::new("clock_unavailable", true))?;
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
        None => None,
    };
    OAuthTokenBundleV1::new(
        registration.provider_id.clone(),
        registration.issuer.clone(),
        response.token_type,
        response.access_token.0,
        refresh_token,
        expires_at,
        refresh_expires_at,
        scopes.into_iter().collect(),
        identity,
        generation,
    )
    .map_err(|_| OAuthPublicError::new("invalid_token_response", false))
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
    let _ = route.sink.try_send(WireFrame::Response {
        request_id: route.request_id.clone(),
        body,
    });
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

const SUCCESS_HTML: &str = "<!doctype html><meta charset=utf-8><meta name=referrer content=no-referrer><title>Haider authorization complete</title><p>Authorization received. Return to Haider.</p>";
const DENIED_HTML: &str = "<!doctype html><meta charset=utf-8><meta name=referrer content=no-referrer><title>Haider authorization cancelled</title><p>Authorization was not granted. Return to Haider.</p>";
const INVALID_HTML: &str = "<!doctype html><meta charset=utf-8><meta name=referrer content=no-referrer><title>Invalid callback</title><p>This callback was rejected.</p>";

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
}

struct RefreshFlight {
    completed: Mutex<Option<Result<(), HaiderError>>>,
    notify: Notify,
}

impl RefreshFlight {
    fn new() -> Self {
        Self {
            completed: Mutex::new(None),
            notify: Notify::new(),
        }
    }

    fn finish(&self, result: Result<(), HaiderError>) {
        if let Ok(mut completed) = self.completed.lock() {
            *completed = Some(result);
        }
        self.notify.notify_waiters();
    }

    async fn wait(&self) -> Result<(), HaiderError> {
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

struct BrokerInner {
    vault: Arc<dyn Vault>,
    catalog: OAuthProviderCatalog,
    status_commands: mpsc::Sender<crate::accounts::AccountCommand>,
    flights: tokio::sync::Mutex<HashMap<RefreshKey, Arc<RefreshFlight>>>,
    fences: Mutex<HashMap<String, Arc<AtomicU64>>>,
    client: reqwest::Client,
    refresh_skew: Duration,
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
}

impl CredentialBroker {
    pub(crate) fn new(
        vault: Arc<dyn Vault>,
        catalog: OAuthProviderCatalog,
        _snapshot: crate::accounts::AccountsSnapshot,
        status_commands: mpsc::Sender<crate::accounts::AccountCommand>,
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
                status_commands,
                flights: tokio::sync::Mutex::new(HashMap::new()),
                fences: Mutex::new(HashMap::new()),
                client,
                refresh_skew: Duration::from_secs(30),
            }),
        })
    }

    pub(crate) async fn resolve(
        &self,
        descriptor: &CredentialDescriptor,
    ) -> Result<SecretHandle, HaiderError> {
        match descriptor.auth_method {
            AuthMethod::ApiKey => self.resolve_vault(&descriptor.alias).await,
            AuthMethod::OAuth => self.resolve_oauth(descriptor).await,
        }
    }

    async fn resolve_oauth(
        &self,
        descriptor: &CredentialDescriptor,
    ) -> Result<SecretHandle, HaiderError> {
        let stored = self.resolve_vault(&descriptor.alias).await?;
        let bundle = match OAuthTokenBundleV1::decode(stored.expose_secret()) {
            Ok(bundle) => bundle,
            Err(error) => {
                self.mark_expired(&descriptor.alias).await?;
                return Err(error);
            }
        };
        if let Err(error) = self.validate_bundle(descriptor, &bundle) {
            self.mark_expired(&descriptor.alias).await?;
            return Err(error);
        }
        let now = now_ms().ok_or_else(|| {
            HaiderError::new(ErrorCode::Internal, "system clock is unavailable", true)
        })?;
        let skew_ms = duration_ms(self.inner.refresh_skew);
        if bundle.expires_at_unix_ms > now.saturating_add(skew_ms) {
            return Ok(bundle.access_token_handle());
        }
        let key = RefreshKey {
            provider: descriptor.provider.clone(),
            alias: descriptor.alias.as_str().to_owned(),
            generation: bundle.generation,
        };
        let (flight, leader) = {
            let mut flights = self.inner.flights.lock().await;
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
            let broker = self.clone();
            let descriptor = descriptor.clone();
            let flight_for_worker = Arc::clone(&flight);
            let key_for_worker = key.clone();
            tokio::spawn(async move {
                let result = broker.refresh(&descriptor, &bundle).await;
                // All waiters observe the same effective outcome. A transient
                // refresh failure may keep using an access token that has not
                // actually expired.
                let public_result = match result {
                    Ok(_) => Ok(()),
                    Err(error)
                        if error.retryable
                            && now_ms()
                                .is_some_and(|current| bundle.expires_at_unix_ms > current) =>
                    {
                        Ok(())
                    }
                    Err(error) => Err(error),
                };
                flight_for_worker.finish(public_result);
                let mut flights = broker.inner.flights.lock().await;
                if flights
                    .get(&key_for_worker)
                    .is_some_and(|current| Arc::ptr_eq(current, &flight_for_worker))
                {
                    flights.remove(&key_for_worker);
                }
            });
        }
        flight.wait().await?;
        let stored = self.resolve_vault(&descriptor.alias).await?;
        let refreshed = match OAuthTokenBundleV1::decode(stored.expose_secret()) {
            Ok(bundle) => bundle,
            Err(error) => {
                self.mark_expired(&descriptor.alias).await?;
                return Err(error);
            }
        };
        if let Err(error) = self.validate_bundle(descriptor, &refreshed) {
            self.mark_expired(&descriptor.alias).await?;
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

    async fn refresh(
        &self,
        descriptor: &CredentialDescriptor,
        bundle: &OAuthTokenBundleV1,
    ) -> Result<SecretHandle, HaiderError> {
        let Some(registration) = self.inner.catalog.registration(&descriptor.provider) else {
            return Err(rotation_error(
                descriptor,
                haider_accounts::RotationTrigger::RefreshFailed,
                false,
                "OAuth registration is unavailable",
            ));
        };
        let Some(refresh_token) = bundle.refresh_token() else {
            self.mark_expired(&descriptor.alias).await?;
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
            self.mark_expired(&descriptor.alias).await?;
            return Err(rotation_error(
                descriptor,
                haider_accounts::RotationTrigger::AuthExpired,
                false,
                "OAuth refresh token has expired",
            ));
        }
        let fence = self.fence_for(&descriptor.alias);
        let expected_fence = fence.load(Ordering::Acquire);
        let response =
            exchange_refresh_token(&self.inner.client, &registration, refresh_token).await;
        let refreshed = match response {
            Ok(bytes) => refresh_bundle_from_response(&registration, &bytes, bundle),
            Err(error) if error.retryable => return Err(oauth_error(error)),
            Err(error) => {
                self.mark_expired(&descriptor.alias).await?;
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
                self.mark_expired(&descriptor.alias).await?;
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
        self.inner
            .status_commands
            .send(crate::accounts::AccountCommand::ApplyOAuthRefresh {
                descriptor: descriptor.clone(),
                expected_generation: bundle.generation,
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
                self.invalidate(&descriptor.alias);
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

    async fn resolve_vault(&self, alias: &CredentialAlias) -> Result<SecretHandle, HaiderError> {
        let vault = Arc::clone(&self.inner.vault);
        let alias = alias.clone();
        tokio::task::spawn_blocking(move || vault.resolve(&alias))
            .await
            .map_err(|_| HaiderError::new(ErrorCode::ProviderError, "vault worker failed", true))?
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
        if bundle.provider_id != descriptor.provider
            || bundle.issuer != registration.issuer
            || !bundle.token_type.eq_ignore_ascii_case("bearer")
            || !registration
                .scopes
                .iter()
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

    fn fence_for(&self, alias: &CredentialAlias) -> Arc<AtomicU64> {
        self.inner
            .fences
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

    /// Removal/replacement fence used by W5c and race tests.
    pub(crate) fn invalidate(&self, alias: &CredentialAlias) {
        self.fence_for(alias).fetch_add(1, Ordering::AcqRel);
    }

    async fn mark_expired(&self, alias: &CredentialAlias) -> Result<(), HaiderError> {
        let (completed, result) = oneshot::channel();
        self.inner
            .status_commands
            .send(crate::accounts::AccountCommand::SetStatus {
                alias: alias.clone(),
                status: CredentialStatus::Expired,
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
}

async fn exchange_refresh_token(
    client: &reqwest::Client,
    registration: &OAuthProviderRegistration,
    refresh_token: &[u8],
) -> Result<Zeroizing<Vec<u8>>, OAuthPublicError> {
    let refresh_token = std::str::from_utf8(refresh_token)
        .map_err(|_| OAuthPublicError::new("invalid_refresh_token", false))?;
    let body = {
        let mut encoded = url::form_urlencoded::Serializer::new(SecretFormBody::empty());
        encoded
            .append_pair("grant_type", "refresh_token")
            .append_pair("refresh_token", refresh_token)
            .append_pair("client_id", &registration.client_id);
        encoded.finish()
    };
    let response = client
        .post(registration.token_endpoint.clone())
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(reqwest::Body::from(bytes::Bytes::from_owner(body)))
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

fn refresh_bundle_from_response(
    registration: &OAuthProviderRegistration,
    bytes: &[u8],
    prior: &OAuthTokenBundleV1,
) -> Result<OAuthTokenBundleV1, OAuthPublicError> {
    let response = serde_json::from_slice::<RefreshTokenResponse>(bytes)
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
    if !registration.scopes.is_subset(&scopes) {
        return Err(OAuthPublicError::new("scope_mismatch", false));
    }
    let now = now_ms().ok_or_else(|| OAuthPublicError::new("clock_unavailable", true))?;
    let refresh_token = match response.refresh_token {
        Some(token) => Some(token.0),
        None if registration.retain_refresh_on_omission => prior
            .refresh_token()
            .map(|token| Zeroizing::new(token.to_vec())),
        None => None,
    };
    OAuthTokenBundleV1::new(
        registration.provider_id.clone(),
        registration.issuer.clone(),
        response.token_type,
        response.access_token.0,
        refresh_token,
        now.checked_add(response.expires_in.saturating_mul(1000))
            .ok_or_else(|| OAuthPublicError::new("invalid_token_response", false))?,
        response
            .refresh_expires_in
            .and_then(|seconds| now.checked_add(seconds.saturating_mul(1000))),
        scopes.into_iter().collect(),
        prior.identity.clone(),
        prior.generation.saturating_add(1),
    )
    .map_err(|_| OAuthPublicError::new("invalid_token_response", false))
}

fn rotation_error(
    descriptor: &CredentialDescriptor,
    trigger: haider_accounts::RotationTrigger,
    retryable: bool,
    message: &'static str,
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
