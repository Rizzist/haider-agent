//! Anthropic Messages API adapter.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use haider_accounts::SecretHandle;
use haider_protocol::ids::CredentialAlias;
use haider_protocol::provider::{CapabilityDoc, FeatureResolve};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderValue, RETRY_AFTER};
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::origin::{FixedDnsResolver, FixedOriginGuard, SystemFixedDnsResolver};
pub use crate::wire::ANTHROPIC_OAUTH_SYSTEM_IDENTITY;
use crate::wire::{
    AnthropicSystemShape, SseDecoder, WireApiError, is_anthropic_context_error, provider_kind_name,
    request_json,
};
use crate::{
    Provider, ProviderError, ProviderErrorKind, ProviderStream, ProviderStreamItem, TurnRequest,
};

pub const ANTHROPIC_PROVIDER_NAME: &str = "anthropic";
pub const ANTHROPIC_OAUTH_PROVIDER_NAME: &str = "anthropic-oauth";
/// G4b enterprise: Claude on AWS Bedrock over the mantle bearer surface.
/// Classic InvokeModel + SigV4 + AWS event-stream are deliberately out of
/// scope — the mantle endpoint speaks the REAL Messages API over standard
/// SSE with an `x-api-key` bearer.
pub const BEDROCK_PROVIDER_NAME: &str = "bedrock";
/// G4b enterprise: Claude on GCP Vertex via `:streamRawPredict`.
pub const VERTEX_PROVIDER_NAME: &str = "vertex";
/// Mantle base URL for the default region (`us-east-1`) — the registry seed.
pub const BEDROCK_MANTLE_DEFAULT_BASE_URL: &str =
    "https://bedrock-mantle.us-east-1.api.aws/anthropic";
/// The `anthropic_version` BODY field Vertex requires in place of `model`.
pub const VERTEX_ANTHROPIC_VERSION: &str = "vertex-2023-10-16";
/// Documented mantle model set (no discovery API exists) — the registry
/// seeds `configured_models` with these; the list stays user-editable.
pub const BEDROCK_SEED_MODELS: [&str; 6] = [
    "anthropic.claude-fable-5",
    "anthropic.claude-opus-5",
    "anthropic.claude-opus-4-8",
    "anthropic.claude-opus-4-7",
    "anthropic.claude-sonnet-5",
    "anthropic.claude-haiku-4-5",
];
/// Documented Claude-on-Vertex model set (no models API) — newer models are
/// plain slugs, older ones carry the Vertex `@date` suffix.
pub const VERTEX_SEED_MODELS: [&str; 5] = [
    "claude-fable-5",
    "claude-opus-5",
    "claude-sonnet-5",
    "claude-sonnet-4-5@20250929",
    "claude-haiku-4-5@20251001",
];
pub const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";
pub const ANTHROPIC_OAUTH_BASE_URL: &str = "https://api.anthropic.com";
pub const ANTHROPIC_OAUTH_BETA_HEADER: &str = "anthropic-beta";
pub const ANTHROPIC_OAUTH_BETA_VALUE: &str = "oauth-2025-04-20";
/// Beta token the fast-mode research preview requires on `anthropic-beta`
/// whenever the body carries `speed: "fast"` (G3). On OAuth requests it is
/// comma-joined AFTER the subscription beta.
pub const ANTHROPIC_FAST_BETA_VALUE: &str = "fast-mode-2026-02-01";
const ANTHROPIC_API_HOST: &str = "api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const STREAM_CAPACITY: usize = 32;
const TRANSPORT_CONFIG: AnthropicTransportConfig = AnthropicTransportConfig {
    retry_policy: AnthropicRetryPolicy::Never,
    connect_timeout: Duration::from_secs(10),
    response_open_timeout: Duration::from_secs(30),
    chunk_idle_timeout: Duration::from_secs(90),
};

/// Retry behavior owned by the Anthropic HTTP adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnthropicRetryPolicy {
    /// Surface each transport failure once; the actor owns any retry/backoff.
    Never,
}

/// Inspectable transport invariants applied by [`AnthropicProvider::new`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnthropicTransportConfig {
    pub retry_policy: AnthropicRetryPolicy,
    pub connect_timeout: Duration,
    pub response_open_timeout: Duration,
    pub chunk_idle_timeout: Duration,
}

/// Messages API provider backed by one already-resolved account secret.
///
/// `SecretHandle` has redacted formatting and cannot be cloned or serialized.
/// Request-local header values are additionally marked sensitive for reqwest.
#[derive(Debug)]
pub struct AnthropicProvider {
    client: reqwest::Client,
    credential: SecretHandle,
    account: Option<CredentialAlias>,
    model: String,
    api_url: String,
    auth_mode: AnthropicAuthMode,
    endpoint_shape: AnthropicEndpointShape,
    fixed_origin_guard: Option<Arc<FixedOriginGuard>>,
    /// Session-selected effort (G3), injected as `output_config.effort`.
    /// The DAEMON gates it at construction: post-switch stale levels clamp
    /// down the documented ladder (`anthropic_effort_clamp`), so the adapter
    /// applies what it is handed verbatim; only models with no documented
    /// ladder still surface the provider's own error.
    effort: Option<String>,
    /// Session-selected fast mode (G3): body `speed: "fast"` plus the
    /// `fast-mode-2026-02-01` beta header. The caller gates this on the
    /// static model table — the adapter applies it verbatim.
    fast: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnthropicAuthMode {
    ApiKey,
    OAuthBearer,
    /// G4b Vertex: a plain `Authorization: Bearer` GCP access token — no
    /// OAuth beta header and no Claude Code system-identity shape.
    CloudBearer,
}

/// Which endpoint dialect the adapter speaks (G4b).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnthropicEndpointShape {
    /// `POST {base}/v1/messages`, model in the body, `anthropic-version`
    /// header. The first-party API and the Bedrock mantle both speak this.
    Standard,
    /// Vertex `:streamRawPredict`: the MODEL rides in the URL, the body
    /// carries `anthropic_version` INSTEAD of `model`, and no
    /// `anthropic-version` header is sent.
    Vertex,
}

/// Raw response returned only to the explicit fixture-promotion harness.
#[derive(Debug)]
pub struct AnthropicCapture {
    pub status: u16,
    pub retry_after: Option<String>,
    pub body: Vec<u8>,
}

impl AnthropicProvider {
    pub fn new(credential: SecretHandle, model: impl Into<String>) -> Result<Self, ProviderError> {
        Self::new_with_auth(credential, model, AnthropicAuthMode::ApiKey, None)
    }

    pub fn new_subscription(
        credential: SecretHandle,
        model: impl Into<String>,
        base_url: &str,
    ) -> Result<Self, ProviderError> {
        Self::new_subscription_with_dns_resolver(
            credential,
            model,
            base_url,
            Arc::new(SystemFixedDnsResolver),
        )
    }

    pub(crate) fn new_subscription_with_dns_resolver(
        credential: SecretHandle,
        model: impl Into<String>,
        base_url: &str,
        resolver: Arc<dyn FixedDnsResolver>,
    ) -> Result<Self, ProviderError> {
        if base_url != ANTHROPIC_OAUTH_BASE_URL {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "Anthropic subscription inference base URL is not sanctioned",
            ));
        }
        let guard = Arc::new(FixedOriginGuard::new(
            ANTHROPIC_API_URL,
            ANTHROPIC_API_HOST,
            resolver,
        )?);
        Self::new_with_auth(
            credential,
            model,
            AnthropicAuthMode::OAuthBearer,
            Some(guard),
        )
    }

    /// Constructs the Bedrock-mantle adapter (G4b, LB2): the base URL must
    /// match the mantle shape `https://bedrock-mantle.{region}.api.aws/
    /// anthropic` EXACTLY — any other endpoint is refused so the `x-api-key`
    /// bearer can never be redirected off the mantle surface. The wire is
    /// the standard Messages dialect at `{base}/v1/messages`.
    pub fn new_endpoint(
        credential: SecretHandle,
        model: impl Into<String>,
        base_url: &str,
    ) -> Result<Self, ProviderError> {
        let base = validate_bedrock_mantle_base_url(base_url)?;
        let mut provider = Self::new_with_auth(credential, model, AnthropicAuthMode::ApiKey, None)?;
        provider.api_url = format!("{base}/v1/messages");
        Ok(provider)
    }

    /// Constructs the Claude-on-Vertex adapter (G4b, LV1): the base URL must
    /// match the Vertex publishers-models shape; the request URL appends
    /// `/{model}:streamRawPredict`, the body drops `model` and carries
    /// `anthropic_version` instead, and auth is a plain GCP Bearer token.
    pub fn new_vertex(
        credential: SecretHandle,
        model: impl Into<String>,
        base_url: &str,
    ) -> Result<Self, ProviderError> {
        let base = validate_vertex_models_base_url(base_url)?;
        let mut provider =
            Self::new_with_auth(credential, model, AnthropicAuthMode::CloudBearer, None)?;
        provider.endpoint_shape = AnthropicEndpointShape::Vertex;
        provider.api_url = format!("{base}/{}:streamRawPredict", provider.model);
        Ok(provider)
    }

    fn new_with_auth(
        credential: SecretHandle,
        model: impl Into<String>,
        auth_mode: AnthropicAuthMode,
        fixed_origin_guard: Option<Arc<FixedOriginGuard>>,
    ) -> Result<Self, ProviderError> {
        let transport = Self::transport_config();
        let mut client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(match transport.retry_policy {
                AnthropicRetryPolicy::Never => reqwest::retry::never(),
            })
            .connect_timeout(transport.connect_timeout);
        if let Some(guard) = &fixed_origin_guard {
            client = client.dns_resolver(Arc::clone(guard));
        }
        let client = client.build().map_err(|error| {
            ProviderError::new(
                ProviderErrorKind::Internal,
                format!("could not construct Anthropic HTTP client: {error}"),
            )
        })?;
        Ok(Self {
            client,
            credential,
            account: None,
            model: model.into(),
            api_url: ANTHROPIC_API_URL.into(),
            auth_mode,
            endpoint_shape: AnthropicEndpointShape::Standard,
            fixed_origin_guard,
            effort: None,
            fast: false,
        })
    }

    /// Returns the exact retry and timeout policy consumed by the constructor
    /// and per-chunk streaming loop.
    #[must_use]
    pub const fn transport_config() -> AnthropicTransportConfig {
        TRANSPORT_CONFIG
    }

    #[must_use]
    pub fn with_account(mut self, account: CredentialAlias) -> Self {
        self.account = Some(account);
        self
    }

    /// Sets the session-selected effort injected as `output_config.effort`.
    #[must_use]
    pub fn with_effort(mut self, effort: Option<String>) -> Self {
        self.effort = effort;
        self
    }

    /// Enables fast mode: `speed: "fast"` in the body plus
    /// [`ANTHROPIC_FAST_BETA_VALUE`] on the `anthropic-beta` header
    /// (comma-joined with the OAuth beta on subscription requests).
    #[must_use]
    pub fn with_fast(mut self, fast: bool) -> Self {
        self.fast = fast;
        self
    }

    /// Overrides the endpoint for an explicit capture/test harness.
    #[must_use]
    pub fn with_api_url(mut self, api_url: impl Into<String>) -> Self {
        self.api_url = api_url.into();
        self
    }

    #[cfg(test)]
    pub(crate) fn client_debug(&self) -> String {
        format!("{:?}", self.client)
    }

    #[cfg(test)]
    pub(crate) fn stall_fixed_connection_resolution(&self) -> bool {
        let Some(guard) = self.fixed_origin_guard.as_ref() else {
            return false;
        };
        guard.stall_connection_resolution();
        true
    }

    #[cfg(test)]
    pub(crate) fn fixed_connection_resolution_count(&self) -> Option<usize> {
        self.fixed_origin_guard
            .as_ref()
            .map(|guard| guard.connection_resolution_count())
    }

    #[cfg(test)]
    pub(crate) async fn execute_request_for_test(
        &self,
        request: reqwest::Request,
    ) -> Result<reqwest::Response, reqwest::Error> {
        self.client.execute(request).await
    }

    /// Builds the secret-free JSON body. Capture tools use this to record the
    /// exact payload shape without gaining access to the credential.
    ///
    /// The body shape follows the provider's auth mode: OAuth-subscription
    /// requests must open `system` with the Claude Code identity block (see
    /// [`ANTHROPIC_OAUTH_SYSTEM_IDENTITY`]); `x-api-key` requests keep the
    /// plain-string system prompt untouched.
    pub fn request_payload(
        &self,
        request: &TurnRequest,
    ) -> Result<serde_json::Value, ProviderError> {
        self.validate_model(request)?;
        let system_shape = match self.auth_mode {
            AnthropicAuthMode::ApiKey | AnthropicAuthMode::CloudBearer => {
                AnthropicSystemShape::ApiKey
            }
            AnthropicAuthMode::OAuthBearer => AnthropicSystemShape::OAuthClaudeCode,
        };
        let mut payload = request_json(request, system_shape, self.effort.as_deref(), self.fast)?;
        // G4b Vertex wire deltas (LV1): the model is URL-addressed, so the
        // body DROPS `model` and carries `anthropic_version` in its place —
        // the Vertex replacement for the standard `anthropic-version`
        // header, which this shape never sends.
        if self.endpoint_shape == AnthropicEndpointShape::Vertex {
            let Some(object) = payload.as_object_mut() else {
                return Err(ProviderError::new(
                    ProviderErrorKind::Internal,
                    "Anthropic request payload was not a JSON object",
                ));
            };
            object.remove("model");
            object.insert(
                "anthropic_version".into(),
                serde_json::Value::String(VERTEX_ANTHROPIC_VERSION.into()),
            );
        }
        Ok(payload)
    }

    /// Records one raw response for the ignored promotion harness.
    ///
    /// This method does not write files, sanitize, or promote provenance.
    pub async fn capture_response(
        &self,
        request: &TurnRequest,
    ) -> Result<AnthropicCapture, ProviderError> {
        let response = self.send_request(request).await?;
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = response.bytes().await.map_err(transport_error)?.to_vec();
        Ok(AnthropicCapture {
            status,
            retry_after,
            body,
        })
    }

    fn validate_model(&self, request: &TurnRequest) -> Result<(), ProviderError> {
        if request.model == self.model {
            Ok(())
        } else {
            Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                format!(
                    "Anthropic provider selected model `{}`, but turn requested `{}`",
                    self.model, request.model
                ),
            ))
        }
    }

    fn api_key_header(&self) -> Result<HeaderValue, ProviderError> {
        let mut value = HeaderValue::from_bytes(self.credential.expose_secret()).map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::Authentication,
                "resolved Anthropic credential is not a valid HTTP header value",
            )
        })?;
        value.set_sensitive(true);
        Ok(value)
    }

    fn authorization_header(&self) -> Result<HeaderValue, ProviderError> {
        let secret = self.credential.expose_secret();
        let mut bytes = Vec::with_capacity(7 + secret.len());
        bytes.extend_from_slice(b"Bearer ");
        bytes.extend_from_slice(secret);
        let result = HeaderValue::from_bytes(&bytes).map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::Authentication,
                "resolved Anthropic OAuth credential is not a valid HTTP header value",
            )
        });
        bytes.fill(0);
        let mut value = result?;
        value.set_sensitive(true);
        Ok(value)
    }

    pub(crate) async fn request(
        &self,
        payload: &serde_json::Value,
    ) -> Result<reqwest::Request, ProviderError> {
        if let Some(guard) = &self.fixed_origin_guard {
            guard.validate_endpoint(&self.api_url).await?;
        }
        let mut request = self
            .client
            .post(&self.api_url)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "text/event-stream");
        // The `anthropic-version` header belongs to the Standard dialect
        // (first-party API and Bedrock mantle alike, LB1); Vertex versions
        // through the `anthropic_version` BODY field instead (LV1).
        if self.endpoint_shape == AnthropicEndpointShape::Standard {
            request = request.header("anthropic-version", ANTHROPIC_VERSION);
        }
        request = match self.auth_mode {
            AnthropicAuthMode::ApiKey => {
                let request = request.header("x-api-key", self.api_key_header()?);
                if self.fast {
                    request.header(ANTHROPIC_OAUTH_BETA_HEADER, ANTHROPIC_FAST_BETA_VALUE)
                } else {
                    request
                }
            }
            AnthropicAuthMode::OAuthBearer => {
                // Fast mode APPENDS its beta to the OAuth beta in ONE
                // comma-joined header value — the server reads the header as
                // a token list and the subscription beta must survive.
                let beta = if self.fast {
                    format!("{ANTHROPIC_OAUTH_BETA_VALUE},{ANTHROPIC_FAST_BETA_VALUE}")
                } else {
                    ANTHROPIC_OAUTH_BETA_VALUE.to_owned()
                };
                request
                    .header(AUTHORIZATION, self.authorization_header()?)
                    .header(ANTHROPIC_OAUTH_BETA_HEADER, beta)
            }
            // G4b Vertex: a bare GCP Bearer token — no beta header ever
            // (fast is Claude-API-only and the factory never sets it here).
            AnthropicAuthMode::CloudBearer => {
                request.header(AUTHORIZATION, self.authorization_header()?)
            }
        };
        request.json(payload).build().map_err(transport_error)
    }

    async fn send_request(
        &self,
        request: &TurnRequest,
    ) -> Result<reqwest::Response, ProviderError> {
        let payload = self.request_payload(request)?;
        let request = self.request(&payload).await?;
        let opening = self.client.execute(request);
        tokio::time::timeout(Self::transport_config().response_open_timeout, opening)
            .await
            .map_err(|_| {
                response_open_timeout_error(Self::transport_config().response_open_timeout)
            })?
            .map_err(transport_error)
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn credential_surface(&self) -> crate::ProviderCredentialSurface {
        match self.auth_mode {
            AnthropicAuthMode::ApiKey => crate::ProviderCredentialSurface::ApiKey,
            AnthropicAuthMode::OAuthBearer => {
                crate::ProviderCredentialSurface::OAuthSubscriptionBearer
            }
            AnthropicAuthMode::CloudBearer => crate::ProviderCredentialSurface::CloudBearer,
        }
    }

    async fn capabilities(&self) -> CapabilityDoc {
        let model = model_capabilities(&self.model);
        CapabilityDoc {
            provider: ANTHROPIC_PROVIDER_NAME.into(),
            parallel_tools: FeatureResolve::Native,
            streaming_tool_args: FeatureResolve::Native,
            vision: FeatureResolve::Native,
            thinking_visible: model.thinking_visible,
            context_limit: model.context_limit,
        }
    }

    async fn stream_turn(&self, request: TurnRequest) -> Result<ProviderStream, ProviderError> {
        let response = self.send_request(&request).await?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let retry_after = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let body = response.bytes().await.map_err(transport_error)?;
            return Err(replay_anthropic_http_error(
                status,
                retry_after.as_deref(),
                &body,
            ));
        }

        let (sender, receiver) = mpsc::channel(STREAM_CAPACITY);
        let account = self.account.clone();
        let chunk_idle_timeout = Self::transport_config().chunk_idle_timeout;
        let producer = tokio::spawn(async move {
            stream_response(response, account, sender, chunk_idle_timeout).await;
        });
        Ok(ProviderStream::owned(receiver, producer))
    }
}

#[derive(Debug, Clone, Copy)]
struct ModelCapabilities {
    context_limit: u64,
    thinking_visible: FeatureResolve,
}

fn model_capabilities(model: &str) -> ModelCapabilities {
    // G4b: enterprise spellings (`anthropic.` prefix, `@date` suffix) carry
    // the same models — normalize before the family match so a Bedrock or
    // Vertex slug reports its real context window.
    let model = crate::effort::base_model(model);
    if model == "claude-fable-5"
        || model.starts_with("claude-opus-5")
        || model.starts_with("claude-sonnet-5")
    {
        ModelCapabilities {
            context_limit: 1_000_000,
            thinking_visible: FeatureResolve::Native,
        }
    } else if model.starts_with("claude-haiku-4-5") {
        ModelCapabilities {
            context_limit: 200_000,
            thinking_visible: FeatureResolve::Native,
        }
    } else {
        ModelCapabilities {
            context_limit: 100_000,
            thinking_visible: FeatureResolve::Unsupported,
        }
    }
}

async fn stream_response(
    response: reqwest::Response,
    account: Option<CredentialAlias>,
    sender: mpsc::Sender<ProviderStreamItem>,
    chunk_idle_timeout: Duration,
) {
    stream_sse_source(response, account, sender, chunk_idle_timeout).await;
}

pub(crate) trait SseChunkSource {
    async fn next_chunk(
        &mut self,
    ) -> Result<Option<impl AsRef<[u8]> + Send + 'static>, ProviderError>;
}

impl SseChunkSource for reqwest::Response {
    async fn next_chunk(
        &mut self,
    ) -> Result<Option<impl AsRef<[u8]> + Send + 'static>, ProviderError> {
        self.chunk().await.map_err(transport_error)
    }
}

pub(crate) async fn stream_sse_source<S: SseChunkSource>(
    mut source: S,
    account: Option<CredentialAlias>,
    sender: mpsc::Sender<ProviderStreamItem>,
    chunk_idle_timeout: Duration,
) {
    let mut decoder = SseDecoder::new(account);
    loop {
        let chunk = match tokio::time::timeout(chunk_idle_timeout, source.next_chunk()).await {
            Ok(Ok(Some(chunk))) => chunk,
            Ok(Ok(None)) => {
                send_items(&sender, decoder.finish()).await;
                return;
            }
            Ok(Err(error)) => {
                let _ = sender.send(Err(error)).await;
                return;
            }
            Err(_) => {
                let _ = sender
                    .send(Err(stream_idle_error(chunk_idle_timeout)))
                    .await;
                return;
            }
        };
        let items = decoder.push(chunk.as_ref());
        if !send_items(&sender, items).await || decoder.is_terminal() {
            return;
        }
    }
}

async fn send_items(
    sender: &mpsc::Sender<ProviderStreamItem>,
    items: Vec<ProviderStreamItem>,
) -> bool {
    for item in items {
        if sender.send(item).await.is_err() {
            return false;
        }
    }
    true
}

fn transport_error(error: reqwest::Error) -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Transport,
        format!("Anthropic HTTP transport failed: {error}"),
    )
}

fn stream_idle_error(timeout: Duration) -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Transport,
        format!(
            "Anthropic SSE stream received no data for {} seconds",
            timeout.as_secs()
        ),
    )
}

fn response_open_timeout_error(timeout: Duration) -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Transport,
        format!(
            "Anthropic response did not open within {} seconds",
            timeout.as_secs()
        ),
    )
}

/// One DNS-safe label: non-empty, bounded, lowercase ASCII letters, digits,
/// and hyphens only — deliberately excludes `.`, `/`, `@`, and `:` so a
/// crafted label can never smuggle extra host or path structure into a
/// templated URL.
fn valid_url_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 63
        && label
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// Pins the Bedrock mantle base-URL shape (G4b, LB2):
/// `https://bedrock-mantle.{region}.api.aws/anthropic` with a DNS-safe
/// region label. Returns the canonical base (trailing slash trimmed).
/// EVERYTHING else — plain http, other hosts, dotted or empty regions,
/// extra path segments — is refused, so [`AnthropicProvider::new_endpoint`]
/// can never aim a bearer credential off the mantle surface.
pub fn validate_bedrock_mantle_base_url(base_url: &str) -> Result<String, ProviderError> {
    let trimmed = base_url.trim().trim_end_matches('/');
    let region = trimmed
        .strip_prefix("https://bedrock-mantle.")
        .and_then(|rest| rest.strip_suffix(".api.aws/anthropic"));
    match region {
        Some(region) if valid_url_label(region) => Ok(trimmed.to_owned()),
        _ => Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "Bedrock endpoint must match https://bedrock-mantle.{region}.api.aws/anthropic",
        )),
    }
}

/// Builds the mantle base URL for one validated region (the TUI card's
/// region field routes through this so URL construction has ONE authority).
pub fn bedrock_mantle_base_url(region: &str) -> Result<String, ProviderError> {
    validate_bedrock_mantle_base_url(&format!(
        "https://bedrock-mantle.{}.api.aws/anthropic",
        region.trim()
    ))
}

/// Pins the Claude-on-Vertex models base-URL shape (G4b, LV1):
/// `https://aiplatform.googleapis.com/v1/projects/{p}/locations/global/publishers/anthropic/models`
/// for the global endpoint, or
/// `https://{loc}-aiplatform.googleapis.com/v1/projects/{p}/locations/{loc}/publishers/anthropic/models`
/// for a regional one (host and path location must AGREE). The adapter
/// appends `/{model}:streamRawPredict`. Returns the canonical base.
pub fn validate_vertex_models_base_url(base_url: &str) -> Result<String, ProviderError> {
    let refused = || {
        ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "Vertex endpoint must match https://{loc-}aiplatform.googleapis.com/v1/projects/{project}/locations/{loc}/publishers/anthropic/models",
        )
    };
    let trimmed = base_url.trim().trim_end_matches('/');
    let rest = trimmed.strip_prefix("https://").ok_or_else(refused)?;
    let (host, path) = rest.split_once('/').ok_or_else(refused)?;
    let host_location = match host.strip_suffix("aiplatform.googleapis.com") {
        Some("") => None,
        Some(prefix) => Some(prefix.strip_suffix('-').ok_or_else(refused)?),
        None => return Err(refused()),
    };
    let project = path
        .strip_prefix("v1/projects/")
        .and_then(|rest| rest.split_once("/locations/"))
        .ok_or_else(refused)?;
    let (project, rest) = project;
    let location = rest
        .strip_suffix("/publishers/anthropic/models")
        .ok_or_else(refused)?;
    if !valid_url_label(project) || !valid_url_label(location) {
        return Err(refused());
    }
    match host_location {
        None if location == "global" => Ok(trimmed.to_owned()),
        Some(host_location) if host_location == location && location != "global" => {
            Ok(trimmed.to_owned())
        }
        _ => Err(refused()),
    }
}

/// Builds the Vertex models base URL from card coordinates (ONE authority
/// for the global/regional template split).
pub fn vertex_models_base_url(project: &str, location: &str) -> Result<String, ProviderError> {
    let project = project.trim();
    let location = location.trim();
    let candidate = if location == "global" || location.is_empty() {
        format!(
            "https://aiplatform.googleapis.com/v1/projects/{project}/locations/global/publishers/anthropic/models"
        )
    } else {
        format!(
            "https://{location}-aiplatform.googleapis.com/v1/projects/{project}/locations/{location}/publishers/anthropic/models"
        )
    };
    validate_vertex_models_base_url(&candidate)
}

/// Replays captured SSE bytes through the same incremental decoder used by
/// live HTTP. Small chunks deliberately exercise arbitrary transport splits.
#[must_use]
pub fn replay_anthropic_sse(bytes: &[u8]) -> Vec<ProviderStreamItem> {
    let mut decoder = SseDecoder::new(None);
    let mut items = Vec::new();
    for chunk in bytes.chunks(7) {
        items.extend(decoder.push(chunk));
        if decoder.is_terminal() {
            return items;
        }
    }
    items.extend(decoder.finish());
    items
}

/// Replays a captured non-success HTTP response through the live classifier.
#[must_use]
pub fn replay_anthropic_http_error(
    status: u16,
    retry_after: Option<&str>,
    body: &[u8],
) -> ProviderError {
    let parsed = serde_json::from_slice::<ErrorEnvelope>(body).ok();
    let body_kind = parsed.as_ref().map(|envelope| envelope.error.kind.as_str());
    let context_exceeded = parsed.as_ref().is_some_and(|envelope| {
        is_anthropic_context_error(&envelope.error.kind, &envelope.error.message)
    });
    let kind = match status {
        401 => ProviderErrorKind::Authentication,
        403 => ProviderErrorKind::PermissionDenied,
        429 => ProviderErrorKind::RateLimited,
        529 => ProviderErrorKind::Overloaded,
        500..=599 => ProviderErrorKind::Transport,
        _ if context_exceeded => ProviderErrorKind::ContextExceeded,
        _ => match body_kind {
            Some("authentication_error") => ProviderErrorKind::Authentication,
            Some("permission_error") => ProviderErrorKind::PermissionDenied,
            Some("rate_limit_error") => ProviderErrorKind::RateLimited,
            Some("overloaded_error") => ProviderErrorKind::Overloaded,
            Some("api_error" | "timeout_error") => ProviderErrorKind::Transport,
            _ => ProviderErrorKind::InvalidRequest,
        },
    };
    let message = format!(
        "Anthropic HTTP {status} returned {}",
        provider_kind_name(kind)
    );
    let retry_after_ms = matches!(
        kind,
        ProviderErrorKind::RateLimited
            | ProviderErrorKind::Overloaded
            | ProviderErrorKind::Transport
    )
    .then(|| parse_retry_after(retry_after))
    .flatten();
    ProviderError::new(kind, message).with_retry_after_ms(retry_after_ms)
}

#[derive(Debug, Deserialize)]
struct ErrorEnvelope {
    error: WireApiError,
}

fn parse_retry_after(value: Option<&str>) -> Option<u64> {
    let value = value?.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return seconds.checked_mul(1_000);
    }
    let retry_at = httpdate::parse_http_date(value).ok()?;
    let now = SystemTime::now();
    let duration = retry_at.duration_since(now).unwrap_or_default();
    u64::try_from(duration.as_millis()).ok()
}
