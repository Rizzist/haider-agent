//! Anthropic Messages API adapter.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use haider_accounts::SecretHandle;
use haider_protocol::error::{ErrorAction, ErrorPresentation, ErrorScope};
use haider_protocol::ids::CredentialAlias;
use haider_protocol::provider::{CapabilityDoc, FeatureResolve};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderValue, RETRY_AFTER};
use serde::Deserialize;
use tokio::sync::mpsc;

/// Anthropic documents this as a limit on the complete JSON request, not the
/// decoded PDF. Check the final payload because base64 expansion and prompt
/// history both count toward it.
const ANTHROPIC_PDF_REQUEST_MAX_BYTES: usize = 32 * 1024 * 1024;

use crate::openai::CustomCompatibleOriginGuard;
#[cfg(test)]
use crate::origin::FixedDnsResolver;
use crate::origin::{FixedOriginGuard, SystemFixedDnsResolver};
pub use crate::wire::ANTHROPIC_OAUTH_SYSTEM_IDENTITY;
use crate::wire::{
    AnthropicSystemShape, SseDecoder, WireApiError, is_anthropic_context_error, provider_kind_name,
    request_json, request_json_with_reply_bindings,
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
/// First-party cache-scope beta sent by the official Claude Code client on
/// subscription-authenticated Messages requests that carry cache controls.
pub const ANTHROPIC_OAUTH_PROMPT_CACHING_BETA_VALUE: &str = "prompt-caching-scope-2026-01-05";
/// Beta paired with every explicit one-hour cache TTL by the official Claude
/// Code client. This is composed only on OAuth cache-bearing requests here;
/// the API-key contract retains its existing header policy.
pub const ANTHROPIC_EXTENDED_CACHE_TTL_BETA_VALUE: &str = "extended-cache-ttl-2025-04-11";
/// Beta token the fast-mode research preview requires on `anthropic-beta`
/// whenever the body carries `speed: "fast"` (G3). On OAuth requests it is
/// comma-joined AFTER the subscription beta.
pub const ANTHROPIC_FAST_BETA_VALUE: &str = "fast-mode-2026-02-01";
/// Beta tokens paired with Anthropic's two model-trained computer-use tool
/// schemas. These are request features, so they compose on the ONE
/// `anthropic-beta` header with OAuth subscription identity and fast mode.
pub const ANTHROPIC_COMPUTER_BETA_20251124: &str = "computer-use-2025-11-24";
pub const ANTHROPIC_COMPUTER_BETA_20250124: &str = "computer-use-2025-01-24";
const ANTHROPIC_API_HOST: &str = "api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const STREAM_CAPACITY: usize = 32;
const TRANSPORT_CONFIG: AnthropicTransportConfig = AnthropicTransportConfig {
    retry_policy: AnthropicRetryPolicy::Never,
    connect_timeout: Duration::from_secs(10),
    response_open_timeout: Duration::from_secs(30),
    chunk_idle_timeout: Duration::from_secs(90),
    semantic_progress_timeout: Duration::from_secs(5 * 60),
};

static ANTHROPIC_CLIENT_BUILD_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Anthropic's model-keyed native computer tool dialect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnthropicComputerToolVersion {
    V20251124,
    V20250124,
}

impl AnthropicComputerToolVersion {
    #[must_use]
    pub const fn tool_type(self) -> &'static str {
        match self {
            Self::V20251124 => "computer_20251124",
            Self::V20250124 => "computer_20250124",
        }
    }

    #[must_use]
    pub const fn beta(self) -> &'static str {
        match self {
            Self::V20251124 => ANTHROPIC_COMPUTER_BETA_20251124,
            Self::V20250124 => ANTHROPIC_COMPUTER_BETA_20250124,
        }
    }
}

/// Resolves only Anthropic's documented computer-use-capable model slugs.
/// Enterprise prefixes and dated release suffixes share the same row; an
/// unknown or future slug deliberately remains on Haider's generic function
/// tool until its native contract is documented.
#[must_use]
pub fn anthropic_computer_tool_version(model: &str) -> Option<AnthropicComputerToolVersion> {
    match crate::effort::base_model(model) {
        "claude-opus-5" | "claude-sonnet-5" | "claude-opus-4-8" | "claude-opus-4-7"
        | "claude-opus-4-6" | "claude-sonnet-4-6" | "claude-opus-4-5" => {
            Some(AnthropicComputerToolVersion::V20251124)
        }
        "claude-sonnet-4-5" | "claude-haiku-4-5" | "claude-opus-4-1" | "claude-sonnet-4"
        | "claude-opus-4" => Some(AnthropicComputerToolVersion::V20250124),
        _ => None,
    }
}

fn anthropic_computer_beta_from_payload(payload: &serde_json::Value) -> Option<&'static str> {
    payload
        .get("tools")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .find_map(
            |tool| match tool.get("type").and_then(serde_json::Value::as_str) {
                Some("computer_20251124") => Some(ANTHROPIC_COMPUTER_BETA_20251124),
                Some("computer_20250124") => Some(ANTHROPIC_COMPUTER_BETA_20250124),
                _ => None,
            },
        )
}

fn build_anthropic_client(
    fixed_origin_guard: Option<Arc<FixedOriginGuard>>,
    compatible_origin_guard: Option<Arc<CustomCompatibleOriginGuard>>,
    transport_config: AnthropicTransportConfig,
) -> Result<reqwest::Client, ProviderError> {
    ANTHROPIC_CLIENT_BUILD_COUNT.fetch_add(1, Ordering::Relaxed);
    let mut client = crate::provider_http_client_builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(match transport_config.retry_policy {
            AnthropicRetryPolicy::Never => reqwest::retry::never(),
        })
        .pool_idle_timeout(crate::PROVIDER_POOL_IDLE_TIMEOUT)
        .http2_adaptive_window(true)
        .connect_timeout(transport_config.connect_timeout);
    if let Some(guard) = fixed_origin_guard {
        client = client.dns_resolver(guard);
    }
    if let Some(guard) = compatible_origin_guard {
        client = client.dns_resolver(guard);
    }
    client.build().map_err(|error| {
        ProviderError::new(
            ProviderErrorKind::Internal,
            format!("could not construct Anthropic HTTP client: {error}"),
        )
    })
}

/// Process-lifetime construction counter used by performance regression
/// harnesses. Production account resolution reuses the complete adapter;
/// standalone constructors deliberately own a configuration-local client.
#[doc(hidden)]
#[must_use]
pub fn anthropic_http_client_build_count() -> usize {
    ANTHROPIC_CLIENT_BUILD_COUNT.load(Ordering::Relaxed)
}

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
    pub semantic_progress_timeout: Duration,
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
    compatible_origin_guard: Option<Arc<CustomCompatibleOriginGuard>>,
    transport_config: AnthropicTransportConfig,
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
    /// W-B: declare the SERVER web tools (`web_search_20250305` +
    /// `web_fetch_20250910`) on every request. The DAEMON gates this per
    /// resolved pair — first-party `anthropic`/`anthropic-oauth` only, never
    /// Bedrock/Vertex, and dropped after a session-scoped degrade.
    web_tools: bool,
    /// Explicit prompt-caching override for capture harnesses and artificial
    /// test models. The first-party API constructor enables it directly;
    /// known consumer-OAuth models are enabled by the verified surface policy
    /// in `prompt_caching_enabled`, while enterprise endpoints remain off.
    prompt_caching_verified: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnthropicCacheTtl {
    FiveMinutes,
    OneHour,
}

impl AnthropicCacheTtl {
    pub(crate) const fn wire(self) -> &'static str {
        match self {
            Self::FiveMinutes => "5m",
            Self::OneHour => "1h",
        }
    }

    const fn milliseconds(self) -> u64 {
        match self {
            Self::FiveMinutes => 5 * 60 * 1_000,
            Self::OneHour => 60 * 60 * 1_000,
        }
    }
}

/// Selects the explicit-cache TTL without guessing future reuse. Unknown or
/// short gaps stay on the lower-cost five-minute write policy.
pub const fn select_anthropic_cache_ttl(
    reuse_gap_ms: Option<u64>,
    expected_later_reads: u32,
) -> AnthropicCacheTtl {
    if matches!(reuse_gap_ms, Some(gap) if gap > 5 * 60 * 1_000) && expected_later_reads >= 2 {
        AnthropicCacheTtl::OneHour
    } else {
        AnthropicCacheTtl::FiveMinutes
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnthropicAuthMode {
    ApiKey,
    None,
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
        let mut provider = Self::new_with_auth(credential, model, AnthropicAuthMode::ApiKey, None)?;
        provider.prompt_caching_verified = true;
        Ok(provider)
    }

    pub fn new_subscription(
        credential: SecretHandle,
        model: impl Into<String>,
        base_url: &str,
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
            Arc::new(SystemFixedDnsResolver),
        )?);
        let client = build_anthropic_client(Some(Arc::clone(&guard)), None, TRANSPORT_CONFIG)?;
        Ok(Self {
            client,
            credential,
            account: None,
            model: model.into(),
            api_url: ANTHROPIC_API_URL.into(),
            auth_mode: AnthropicAuthMode::OAuthBearer,
            endpoint_shape: AnthropicEndpointShape::Standard,
            fixed_origin_guard: Some(guard),
            compatible_origin_guard: None,
            transport_config: TRANSPORT_CONFIG,
            effort: None,
            fast: false,
            web_tools: false,
            prompt_caching_verified: false,
        })
    }

    #[cfg(test)]
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

    /// Constructs a user-owned standard Messages endpoint under the custom
    /// TrustedLan origin policy.
    pub fn new_custom(
        credential: SecretHandle,
        model: impl Into<String>,
        base_url: &str,
    ) -> Result<Self, ProviderError> {
        Self::new_custom_with_auth(credential, model, base_url, AnthropicAuthMode::ApiKey)
    }

    /// Constructs a user-owned standard Messages endpoint without emitting
    /// any credential header.
    pub fn new_custom_no_auth(
        credential: SecretHandle,
        model: impl Into<String>,
        base_url: &str,
    ) -> Result<Self, ProviderError> {
        Self::new_custom_with_auth(credential, model, base_url, AnthropicAuthMode::None)
    }

    fn new_custom_with_auth(
        credential: SecretHandle,
        model: impl Into<String>,
        base_url: &str,
        auth_mode: AnthropicAuthMode,
    ) -> Result<Self, ProviderError> {
        let (base_url, guard) = CustomCompatibleOriginGuard::for_base_url(base_url)?;
        let api_root = if base_url.ends_with("/v1") {
            base_url
        } else {
            format!("{base_url}/v1")
        };
        let client = build_anthropic_client(None, Some(Arc::clone(&guard)), TRANSPORT_CONFIG)?;
        Ok(Self {
            client,
            credential,
            account: None,
            model: model.into(),
            api_url: format!("{api_root}/messages"),
            auth_mode,
            endpoint_shape: AnthropicEndpointShape::Standard,
            fixed_origin_guard: None,
            compatible_origin_guard: Some(guard),
            transport_config: TRANSPORT_CONFIG,
            effort: None,
            fast: false,
            web_tools: false,
            prompt_caching_verified: false,
        })
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
        let client = build_anthropic_client(fixed_origin_guard.clone(), None, TRANSPORT_CONFIG)?;
        Ok(Self {
            client,
            credential,
            account: None,
            model: model.into(),
            api_url: ANTHROPIC_API_URL.into(),
            auth_mode,
            endpoint_shape: AnthropicEndpointShape::Standard,
            fixed_origin_guard,
            compatible_origin_guard: None,
            transport_config: TRANSPORT_CONFIG,
            effort: None,
            fast: false,
            web_tools: false,
            prompt_caching_verified: false,
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

    pub fn with_transport_config(
        mut self,
        transport_config: AnthropicTransportConfig,
    ) -> Result<Self, ProviderError> {
        if transport_config.connect_timeout.is_zero()
            || transport_config.response_open_timeout.is_zero()
            || transport_config.chunk_idle_timeout.is_zero()
            || transport_config.semantic_progress_timeout.is_zero()
        {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "Anthropic transport timeouts must be greater than zero",
            ));
        }
        self.client = build_anthropic_client(
            self.fixed_origin_guard.clone(),
            self.compatible_origin_guard.clone(),
            transport_config,
        )?;
        self.transport_config = transport_config;
        Ok(self)
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

    /// Declares the Anthropic SERVER web tools on every request (W-B). The
    /// caller gates this per resolved pair; the adapter applies it verbatim.
    #[must_use]
    pub fn with_web_tools(mut self, web_tools: bool) -> Self {
        self.web_tools = web_tools;
        self
    }

    /// Installs the result of an auth/model cache capability probe. Known
    /// production OAuth models no longer require this override; it remains the
    /// explicit seam for capture harnesses and artificial test models.
    #[must_use]
    pub fn with_prompt_caching_verified(mut self, verified: bool) -> Self {
        self.prompt_caching_verified = verified;
        self
    }

    /// Overrides the endpoint for an explicit capture/test harness.
    #[must_use]
    pub fn with_api_url(mut self, api_url: impl Into<String>) -> Self {
        self.api_url = api_url.into();
        self
    }

    fn prompt_caching_enabled(&self, model: &str) -> bool {
        self.prompt_caching_verified
            || matches!(self.auth_mode, AnthropicAuthMode::OAuthBearer)
                && verified_anthropic_cache_model(model)
    }

    fn cache_ttl(&self, metadata: &crate::PromptCacheMetadata) -> AnthropicCacheTtl {
        if matches!(self.auth_mode, AnthropicAuthMode::OAuthBearer)
            && verified_anthropic_cache_model(&self.model)
        {
            // Claude Code's first-party subscription path automatically uses
            // the extended cache. Keep API-key economics on the existing
            // metadata-driven selector; this is an OAuth-only contract.
            AnthropicCacheTtl::OneHour
        } else {
            select_anthropic_cache_ttl(metadata.reuse_gap_ms, metadata.expected_later_reads)
        }
    }

    fn request_cache_ttl(&self, request: &TurnRequest) -> Option<AnthropicCacheTtl> {
        self.prompt_caching_enabled(&request.model)
            .then_some(request.cache_metadata.as_ref())
            .flatten()
            .filter(|metadata| {
                metadata.boundaries_valid(request.messages.len())
                    && metadata.account_scope.is_some()
                    && match self.auth_mode {
                        AnthropicAuthMode::ApiKey => metadata.provider == ANTHROPIC_PROVIDER_NAME,
                        AnthropicAuthMode::None => true,
                        AnthropicAuthMode::OAuthBearer => {
                            metadata.provider == ANTHROPIC_OAUTH_PROVIDER_NAME
                        }
                        AnthropicAuthMode::CloudBearer => false,
                    }
                    && known_anthropic_cache_model(&request.model)
            })
            .map(|metadata| self.cache_ttl(metadata))
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

    fn render_payload(
        &self,
        request: &TurnRequest,
        tools: &[crate::ToolDefinition],
        cache_ttl: Option<AnthropicCacheTtl>,
    ) -> Result<serde_json::Value, ProviderError> {
        self.render_payload_inner(request, tools, cache_ttl, false)
            .map(|(payload, _)| payload)
    }

    fn render_payload_inner(
        &self,
        request: &TurnRequest,
        tools: &[crate::ToolDefinition],
        cache_ttl: Option<AnthropicCacheTtl>,
        bind_large_replies: bool,
    ) -> Result<(serde_json::Value, Vec<crate::PreparedReplyBinding>), ProviderError> {
        let system_shape = match self.auth_mode {
            AnthropicAuthMode::ApiKey
            | AnthropicAuthMode::None
            | AnthropicAuthMode::CloudBearer => AnthropicSystemShape::ApiKey,
            AnthropicAuthMode::OAuthBearer => AnthropicSystemShape::OAuthClaudeCode,
        };
        let (mut payload, reply_bindings) = if bind_large_replies {
            request_json_with_reply_bindings(
                request,
                tools,
                system_shape,
                self.effort.as_deref(),
                self.fast,
                self.web_tools,
                None,
            )?
        } else {
            (
                request_json(
                    request,
                    tools,
                    system_shape,
                    self.effort.as_deref(),
                    self.fast,
                    self.web_tools,
                    None,
                )?,
                Vec::new(),
            )
        };
        if let Some(cache_ttl) = cache_ttl {
            apply_anthropic_cache_controls(
                request,
                &mut payload,
                system_shape,
                self.web_tools,
                !tools.is_empty(),
                cache_ttl,
            );
        }
        // G4b Vertex wire deltas (LV1): the model is URL-addressed, so the
        // body DROPS `model` and carries `anthropic_version` in its place.
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
        Ok((payload, reply_bindings))
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
        let cache_ttl = self.request_cache_ttl(request);
        let payload = self.render_payload(request, &request.tools, cache_ttl)?;
        self.validate_pdf_request_size(request, &payload)?;
        Ok(payload)
    }

    fn validate_pdf_request_size(
        &self,
        request: &TurnRequest,
        payload: &serde_json::Value,
    ) -> Result<(), ProviderError> {
        let has_pdf = request.messages.iter().any(|message| {
            message.blocks.iter().any(|block| {
                matches!(
                    block,
                    haider_protocol::provider::Block::Attachment(
                        haider_protocol::tool::AttachmentBlock::Pdf { .. }
                    )
                )
            })
        });
        if has_pdf {
            let payload_bytes = crate::exact_wire_size(payload).ok_or_else(|| {
                ProviderError::new(
                    ProviderErrorKind::Internal,
                    "could not measure Anthropic PDF request",
                )
            })?;
            if payload_bytes > u64::try_from(ANTHROPIC_PDF_REQUEST_MAX_BYTES).unwrap_or(u64::MAX) {
                return Err(ProviderError::new(
                    ProviderErrorKind::InvalidRequest,
                    "Anthropic PDF request exceeds the provider's 32 MiB request limit",
                )
                .with_presentation(ErrorPresentation::new(
                    "pdf-provider-request-too-large",
                    "PDF request is too large for Anthropic",
                    "The complete request, including the base64 PDF and conversation history, exceeds Anthropic's 32 MiB limit. Attach a smaller PDF or start a fresh session.",
                    ErrorScope::Turn,
                    [ErrorAction::RetryFresh],
                )));
            }
        }
        Ok(())
    }

    fn cache_control_observation(
        &self,
        request: &TurnRequest,
        emitted: bool,
    ) -> haider_protocol::provider::CacheControlObservationV1 {
        use haider_protocol::provider::{CacheControlObservationV1, CacheControlOmissionReasonV1};

        if emitted {
            let ttl = request
                .cache_metadata
                .as_ref()
                .map(|metadata| self.cache_ttl(metadata).milliseconds());
            return CacheControlObservationV1::Emitted { ttl_ms: ttl };
        }
        if !self.prompt_caching_enabled(&request.model) {
            return CacheControlObservationV1::NotEmitted {
                reason: CacheControlOmissionReasonV1::Unverified,
            };
        }
        let Some(metadata) = request.cache_metadata.as_ref() else {
            return CacheControlObservationV1::NotEmitted {
                reason: CacheControlOmissionReasonV1::AdapterUnavailable,
            };
        };
        let reason = if !metadata.boundaries_valid(request.messages.len()) {
            CacheControlOmissionReasonV1::InvalidBoundaries
        } else if metadata.account_scope.is_none() {
            CacheControlOmissionReasonV1::MissingAccountScope
        } else if !matches!(
            (self.auth_mode, metadata.provider.as_str()),
            (AnthropicAuthMode::ApiKey, ANTHROPIC_PROVIDER_NAME)
                | (
                    AnthropicAuthMode::OAuthBearer,
                    ANTHROPIC_OAUTH_PROVIDER_NAME
                )
        ) {
            CacheControlOmissionReasonV1::ProviderMismatch
        } else if !known_anthropic_cache_model(&request.model) {
            CacheControlOmissionReasonV1::UnsupportedModel
        } else {
            CacheControlOmissionReasonV1::AdapterUnavailable
        };
        CacheControlObservationV1::NotEmitted { reason }
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
        let body = if response.status().is_success() {
            response.bytes().await.map_err(transport_error)?.to_vec()
        } else {
            read_error_body_bounded(response).await.map_err(|error| {
                classify_http_body_read_error(status, retry_after.as_deref(), error)
                    .with_http_metadata(status, None)
            })?
        };
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

    #[cfg(test)]
    pub(crate) async fn request_body(
        &self,
        payload: serde_json::Value,
    ) -> Result<reqwest::Request, ProviderError> {
        self.request_body_prepared(crate::PreparedWire {
            payload,
            history_boundary: None,
            reply_bindings: Vec::new(),
        })
        .await
    }

    async fn request_body_prepared(
        &self,
        prepared: crate::PreparedWire,
    ) -> Result<reqwest::Request, ProviderError> {
        let request = self.request_builder(&prepared.payload).await?;
        let body = crate::serialize_prepared_json_body(prepared)?;
        request.body(body).build().map_err(transport_error)
    }

    async fn request_builder(
        &self,
        payload: &serde_json::Value,
    ) -> Result<reqwest::RequestBuilder, ProviderError> {
        if let Some(guard) = &self.fixed_origin_guard {
            tokio::time::timeout(
                self.transport_config.connect_timeout,
                guard.validate_endpoint(&self.api_url),
            )
            .await
            .map_err(|_| {
                anthropic_connect_timeout_error(self.transport_config.connect_timeout)
            })??;
        }
        if let Some(guard) = &self.compatible_origin_guard {
            tokio::time::timeout(
                self.transport_config.connect_timeout,
                guard.validate_endpoint(&self.api_url),
            )
            .await
            .map_err(|_| {
                anthropic_connect_timeout_error(self.transport_config.connect_timeout)
            })??;
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
        let computer_beta = anthropic_computer_beta_from_payload(payload);
        request = match self.auth_mode {
            AnthropicAuthMode::ApiKey => {
                let request = request.header("x-api-key", self.api_key_header()?);
                let mut betas = Vec::new();
                if self.fast {
                    betas.push(ANTHROPIC_FAST_BETA_VALUE);
                }
                betas.extend(computer_beta);
                if betas.is_empty() {
                    request
                } else {
                    request.header(ANTHROPIC_OAUTH_BETA_HEADER, betas.join(","))
                }
            }
            AnthropicAuthMode::None => request,
            AnthropicAuthMode::OAuthBearer => {
                // Optional feature betas APPEND after the OAuth identity in
                // ONE comma-joined header — the subscription token must
                // survive every composition.
                let mut betas = vec![ANTHROPIC_OAUTH_BETA_VALUE];
                if payload_uses_cache_ttl(payload, AnthropicCacheTtl::OneHour) {
                    // Claude Code 2.1.238 sends both tokens on its accepted
                    // one-hour subscription-cache requests. They are scoped
                    // to OAuth payloads that actually contain a 1h marker;
                    // API-key headers and uncached OAuth turns are unchanged.
                    betas.push(ANTHROPIC_OAUTH_PROMPT_CACHING_BETA_VALUE);
                    betas.push(ANTHROPIC_EXTENDED_CACHE_TTL_BETA_VALUE);
                }
                if self.fast {
                    betas.push(ANTHROPIC_FAST_BETA_VALUE);
                }
                betas.extend(computer_beta);
                request
                    .header(AUTHORIZATION, self.authorization_header()?)
                    .header(ANTHROPIC_OAUTH_BETA_HEADER, betas.join(","))
            }
            // G4b Vertex: a bare GCP Bearer token. Vertex keeps feature beta
            // headers (the official Anthropic Vertex client does the same),
            // while only `anthropic-version` moves into the request body.
            // Fast remains Claude-API-only and the factory never sets it here.
            AnthropicAuthMode::CloudBearer => {
                let request = request.header(AUTHORIZATION, self.authorization_header()?);
                if let Some(computer_beta) = computer_beta {
                    request.header(ANTHROPIC_OAUTH_BETA_HEADER, computer_beta)
                } else {
                    request
                }
            }
        };
        crate::apply_provider_request_headers(request)
    }

    async fn send_request(
        &self,
        request: &TurnRequest,
    ) -> Result<reqwest::Response, ProviderError> {
        let prepared = match crate::take_prepared_wire_payload() {
            Some(prepared) => prepared,
            None => crate::PreparedWire {
                payload: self.request_payload(request)?,
                history_boundary: None,
                reply_bindings: Vec::new(),
            },
        };
        let request = self.request_body_prepared(prepared).await?;
        let route_gating = self.route_gating();
        let opening = self.client.execute(request);
        crate::route_gated_timeout(
            self.transport_config.response_open_timeout,
            opening,
            route_gating,
        )
        .await
        .map_err(|_| response_open_timeout_error(self.transport_config.response_open_timeout))?
        .map_err(|error| transport_error_for_route(error, route_gating))
    }

    fn route_gating(&self) -> crate::RouteGating {
        if self.compatible_origin_guard.is_some() {
            // A user-owned compatible origin may be a loopback/LAN service or
            // a locally resolved hostname. A missing default route says
            // nothing authoritative about that endpoint.
            crate::RouteGating::Disabled
        } else {
            crate::RouteGating::for_endpoint(&self.api_url)
        }
    }

    async fn stream_turn_ref(
        &self,
        request: &TurnRequest,
    ) -> Result<ProviderStream, ProviderError> {
        let native_computer = anthropic_computer_tool_version(&request.model).is_some()
            && request.tools.iter().any(|tool| tool.name == "computer");
        let response = self.send_request(request).await?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let request_id = response
                .headers()
                .get("request-id")
                .or_else(|| response.headers().get("x-request-id"))
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let retry_after = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let error = match read_error_body_bounded(response).await {
                Ok(body) => replay_anthropic_http_error(status, retry_after.as_deref(), &body),
                Err(error) => classify_http_body_read_error(status, retry_after.as_deref(), error),
            };
            return Err(error.with_http_metadata(status, request_id.as_deref()));
        }

        let (sender, receiver) = mpsc::channel(STREAM_CAPACITY);
        let account = self.account.clone();
        let chunk_idle_timeout = self.transport_config.chunk_idle_timeout;
        let semantic_progress_timeout = self.transport_config.semantic_progress_timeout;
        let context = crate::SseRequestContext::capture(self.route_gating());
        let producer = tokio::spawn(async move {
            stream_response(
                response,
                account,
                sender,
                chunk_idle_timeout,
                semantic_progress_timeout,
                native_computer,
                context,
            )
            .await;
        });
        Ok(ProviderStream::owned(receiver, producer))
    }
}

fn anthropic_cache_control(ttl: AnthropicCacheTtl) -> serde_json::Value {
    serde_json::json!({"type": "ephemeral", "ttl": ttl.wire()})
}

fn anthropic_cache_controls_would_emit(
    request: &TurnRequest,
    payload: &serde_json::Value,
    plan: &crate::InlineBreakpointPlan,
) -> bool {
    if plan.mark_system && request.system_prompt.is_some() {
        return true;
    }
    if plan.mark_final_tool
        && payload
            .get("tools")
            .and_then(serde_json::Value::as_array)
            .and_then(|tools| tools.last())
            .is_some_and(serde_json::Value::is_object)
    {
        return true;
    }
    let Some(messages) = payload
        .get("messages")
        .and_then(serde_json::Value::as_array)
    else {
        return false;
    };
    plan.history_ends.iter().any(|boundary| {
        *boundary > 0
            && *boundary <= messages.len()
            && *boundary <= request.messages.len()
            && request.messages[*boundary - 1]
                .blocks
                .last()
                .is_some_and(|block| {
                    !matches!(
                        block,
                        haider_protocol::provider::Block::ProviderOpaque { .. }
                    )
                })
            && messages[*boundary - 1]
                .get("content")
                .and_then(serde_json::Value::as_array)
                .and_then(|content| content.last())
                .is_some_and(serde_json::Value::is_object)
    })
}

/// Preserves the prior diagnostic comparer exactly without retaining a
/// second neutral DOM. The legacy recursive comparison could see inserted
/// keys only when the surrounding JSON shape stayed the same; notably, an
/// API-key system marker changes `system` from a string to an array and was
/// therefore intentionally invisible to that observation path.
fn anthropic_cache_controls_legacy_observable(
    request: &TurnRequest,
    payload: &serde_json::Value,
    plan: &crate::InlineBreakpointPlan,
) -> bool {
    if plan.mark_system
        && request.system_prompt.is_some()
        && payload
            .get("system")
            .is_some_and(serde_json::Value::is_array)
    {
        return true;
    }
    if plan.mark_final_tool
        && payload
            .get("tools")
            .and_then(serde_json::Value::as_array)
            .and_then(|tools| tools.last())
            .is_some_and(serde_json::Value::is_object)
    {
        return true;
    }
    let Some(messages) = payload
        .get("messages")
        .and_then(serde_json::Value::as_array)
    else {
        return false;
    };
    plan.history_ends.iter().any(|boundary| {
        *boundary > 0
            && *boundary <= messages.len()
            && *boundary <= request.messages.len()
            && request.messages[*boundary - 1]
                .blocks
                .last()
                .is_some_and(|block| {
                    !matches!(
                        block,
                        haider_protocol::provider::Block::ProviderOpaque { .. }
                    )
                })
            && messages[*boundary - 1]
                .get("content")
                .and_then(serde_json::Value::as_array)
                .and_then(|content| content.last())
                .is_some_and(serde_json::Value::is_object)
    })
}

/// Adds Anthropic's cache markers as a thin overlay on the one cache-neutral
/// render. Every target is Haider-shaped; a provider-opaque block at a
/// boundary remains untouched exactly as in the original renderer.
fn apply_anthropic_cache_controls(
    request: &TurnRequest,
    payload: &mut serde_json::Value,
    system_shape: AnthropicSystemShape,
    web_tools: bool,
    has_tools: bool,
    ttl: AnthropicCacheTtl,
) -> bool {
    let Some(metadata) = request.cache_metadata.as_ref() else {
        return false;
    };
    let plan = crate::plan_inline_breakpoints(
        &metadata.provider,
        &request.model,
        &request.messages,
        metadata.cacheable_history_end(),
        metadata.previous_stable_history_end,
        metadata.latest_compaction_summary_end,
        request.system_prompt.is_some(),
        has_tools || web_tools,
        metadata.stable_prefix_tokens,
    );
    let Some(object) = payload.as_object_mut() else {
        return false;
    };
    let mut emitted = false;
    if plan.mark_system
        && let Some(system) = request.system_prompt.as_ref()
    {
        match system_shape {
            AnthropicSystemShape::ApiKey => {
                object.insert(
                    "system".into(),
                    serde_json::json!([{
                        "type": "text",
                        "text": system,
                        "cache_control": anthropic_cache_control(ttl),
                    }]),
                );
                emitted = true;
            }
            AnthropicSystemShape::OAuthClaudeCode => {
                if let Some(system) = object
                    .get_mut("system")
                    .and_then(serde_json::Value::as_array_mut)
                    .and_then(|blocks| blocks.last_mut())
                    .and_then(serde_json::Value::as_object_mut)
                {
                    system.insert("cache_control".into(), anthropic_cache_control(ttl));
                    emitted = true;
                }
            }
        }
    }
    if plan.mark_final_tool
        && let Some(tool) = object
            .get_mut("tools")
            .and_then(serde_json::Value::as_array_mut)
            .and_then(|tools| tools.last_mut())
            .and_then(serde_json::Value::as_object_mut)
    {
        tool.insert("cache_control".into(), anthropic_cache_control(ttl));
        emitted = true;
    }
    if let Some(messages) = object
        .get_mut("messages")
        .and_then(serde_json::Value::as_array_mut)
    {
        for boundary in plan.history_ends {
            if boundary == 0 || boundary > messages.len() || boundary > request.messages.len() {
                continue;
            }
            if request.messages[boundary - 1]
                .blocks
                .last()
                .is_none_or(|block| {
                    matches!(
                        block,
                        haider_protocol::provider::Block::ProviderOpaque { .. }
                    )
                })
            {
                continue;
            }
            if let Some(content) = messages[boundary - 1]
                .get_mut("content")
                .and_then(serde_json::Value::as_array_mut)
                .and_then(|content| content.last_mut())
                .and_then(serde_json::Value::as_object_mut)
            {
                content.insert("cache_control".into(), anthropic_cache_control(ttl));
                emitted = true;
            }
        }
    }
    emitted
}

fn verified_anthropic_cache_model(model: &str) -> bool {
    let model = crate::effort::base_model(model);
    model.starts_with("claude-opus-5")
        || model.starts_with("claude-sonnet-5")
        || model == "claude-fable-5"
        || model.starts_with("claude-opus-4")
        || model.starts_with("claude-sonnet-4")
        || model.starts_with("claude-haiku-4")
        || model.starts_with("claude-3-")
}

fn known_anthropic_cache_model(model: &str) -> bool {
    verified_anthropic_cache_model(model) || cfg!(test) && model == "claude-audit"
}

fn payload_uses_cache_ttl(payload: &serde_json::Value, ttl: AnthropicCacheTtl) -> bool {
    match payload {
        serde_json::Value::Array(values) => values
            .iter()
            .any(|value| payload_uses_cache_ttl(value, ttl)),
        serde_json::Value::Object(values) => {
            let matches_here = values.get("cache_control").is_some_and(|cache_control| {
                cache_control
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    == Some("ephemeral")
                    && cache_control.get("ttl").and_then(serde_json::Value::as_str)
                        == Some(ttl.wire())
            });
            matches_here
                || values
                    .values()
                    .any(|value| payload_uses_cache_ttl(value, ttl))
        }
        _ => false,
    }
}

impl AnthropicProvider {
    fn prepare_turn_with_tools_inner(
        &self,
        request: &TurnRequest,
        tools: &[crate::ToolDefinition],
        attachment_moves: Option<&mut [crate::StagedAttachmentMove]>,
    ) -> Option<crate::PreparedTurn> {
        let boundary = request.cache_metadata.as_ref()?.cacheable_history_end();
        self.validate_model(request).ok()?;
        let (rendered_payload, reply_bindings) =
            self.render_payload_inner(request, tools, None, true).ok()?;
        let mut full_payload =
            crate::AttachmentMovePayload::new(rendered_payload, attachment_moves);
        let metadata = request.cache_metadata.as_ref()?;
        let ledger_plan = crate::plan_inline_breakpoints(
            &metadata.provider,
            &request.model,
            &request.messages,
            metadata.cacheable_history_end(),
            metadata.previous_stable_history_end,
            metadata.latest_compaction_summary_end,
            request.system_prompt.is_some(),
            !tools.is_empty(),
            metadata.stable_prefix_tokens,
        );
        let wire_plan = crate::plan_inline_breakpoints(
            &metadata.provider,
            &request.model,
            &request.messages,
            metadata.cacheable_history_end(),
            metadata.previous_stable_history_end,
            metadata.latest_compaction_summary_end,
            request.system_prompt.is_some(),
            !tools.is_empty() || self.web_tools,
            metadata.stable_prefix_tokens,
        );
        let cache_ttl = self.request_cache_ttl(request);
        let will_emit = cache_ttl.is_some()
            && anthropic_cache_controls_would_emit(request, &full_payload, &wire_plan);
        let legacy_observable = cache_ttl.is_some()
            && anthropic_cache_controls_legacy_observable(request, &full_payload, &wire_plan);
        let boundaries = if legacy_observable {
            ledger_plan.ledger_boundaries()
        } else {
            Vec::new()
        };
        let messages = full_payload.get("messages")?.as_array()?;
        let (history_blocks, previous_history_blocks, previous_history_block_len) =
            crate::serialized_provider_view_history(
                messages,
                0,
                boundary,
                metadata.previous_stable_history_end,
                &reply_bindings,
            )?;
        let mut provider_view = crate::cachemaxxing::prepared_serialized_provider_view(
            request,
            "anthropic_messages",
            full_payload.get("system"),
            full_payload.get("tools"),
            history_blocks,
            previous_history_blocks,
            boundaries,
        )?;
        let (prefix_digests, mut previous_immutable_history_digest, provider_view_storage_blobs) =
            crate::rendered_prefix_digests_from_provider_view(
                request,
                &mut provider_view,
                false,
                previous_history_block_len,
            )?;
        if previous_immutable_history_digest.is_none() {
            previous_immutable_history_digest = metadata
                .previous_stable_history_end
                .filter(|previous| *previous <= messages.len())
                .and_then(|previous| {
                    crate::exact_json_digest_with_replies(
                        &Some(&messages[..previous]),
                        &reply_bindings,
                    )
                });
        }
        let emitted = cache_ttl.is_some_and(|ttl| {
            apply_anthropic_cache_controls(
                request,
                &mut full_payload,
                match self.auth_mode {
                    AnthropicAuthMode::ApiKey
                    | AnthropicAuthMode::None
                    | AnthropicAuthMode::CloudBearer => AnthropicSystemShape::ApiKey,
                    AnthropicAuthMode::OAuthBearer => AnthropicSystemShape::OAuthClaudeCode,
                },
                self.web_tools,
                !tools.is_empty(),
                ttl,
            )
        });
        debug_assert_eq!(emitted, will_emit);
        self.validate_pdf_request_size(request, &full_payload)
            .ok()?;
        let cache_control = self.cache_control_observation(request, legacy_observable);
        Some(crate::PreparedTurn {
            prefix_digests,
            previous_immutable_history_digest,
            cache_control,
            provider_view: Some(provider_view),
            provider_view_storage_blobs,
            wire: Some(crate::PreparedWire {
                payload: full_payload.commit(),
                history_boundary: None,
                reply_bindings,
            }),
            turn_trace: None,
        })
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn trusts_default_route_absence(&self) -> bool {
        self.route_gating().enabled()
    }

    fn credential_surface(&self) -> crate::ProviderCredentialSurface {
        match self.auth_mode {
            AnthropicAuthMode::ApiKey => crate::ProviderCredentialSurface::ApiKey,
            AnthropicAuthMode::None => crate::ProviderCredentialSurface::ApiKey,
            AnthropicAuthMode::OAuthBearer => {
                crate::ProviderCredentialSurface::OAuthSubscriptionBearer
            }
            AnthropicAuthMode::CloudBearer => crate::ProviderCredentialSurface::CloudBearer,
        }
    }

    fn usage_lane_dimensions(&self) -> haider_protocol::provider::UsageLaneDimensions {
        let speed = matches!(self.auth_mode, AnthropicAuthMode::OAuthBearer)
            .then(|| {
                crate::effort::anthropic_fast_mode_supported(&self.model)
                    .then(|| if self.fast { "fast" } else { "standard" }.to_owned())
            })
            .flatten();
        haider_protocol::provider::UsageLaneDimensions {
            api_family: Some("anthropic_messages".into()),
            effort: self.effort.clone(),
            // API-key and cloud lanes deliberately have no speed tier in the
            // usage contract, even if a future wire surface grows one.
            speed,
        }
    }

    fn rendered_cache_prefix_digests(
        &self,
        request: &TurnRequest,
    ) -> Option<haider_protocol::provider::PrefixDigests> {
        self.prepare_turn(request)
            .map(|prepared| prepared.prefix_digests)
    }

    fn prepare_turn(&self, request: &TurnRequest) -> Option<crate::PreparedTurn> {
        <Self as Provider>::prepare_turn_with_tools(self, request, &request.tools)
    }

    fn prepare_turn_with_tools(
        &self,
        request: &TurnRequest,
        tools: &[crate::ToolDefinition],
    ) -> Option<crate::PreparedTurn> {
        self.prepare_turn_with_tools_inner(request, tools, None)
    }

    fn prepare_turn_owned(&self, request: &mut TurnRequest) -> Option<crate::PreparedTurn> {
        let tools = request.tools.clone();
        <Self as Provider>::prepare_turn_with_tools_owned(self, request, &tools)
    }

    fn prepare_turn_with_tools_owned(
        &self,
        request: &mut TurnRequest,
        tools: &[crate::ToolDefinition],
    ) -> Option<crate::PreparedTurn> {
        let mut attachment_moves = crate::stage_attachment_moves(request)?;
        let prepared = self.prepare_turn_with_tools_inner(
            request,
            tools,
            Some(attachment_moves.as_mut_slice()),
        );
        crate::restore_attachment_moves(request, &mut attachment_moves);
        prepared
    }

    fn claim_prewarm(&self) -> bool {
        crate::claim_optional_http_prewarm(&self.api_url)
    }

    async fn prewarm(&self) {
        crate::optional_http_prewarm(&self.client, &self.api_url).await;
    }

    async fn capabilities(&self) -> CapabilityDoc {
        let model = model_capabilities(&self.model);
        CapabilityDoc {
            provider: ANTHROPIC_PROVIDER_NAME.into(),
            parallel_tools: FeatureResolve::Native,
            streaming_tool_args: FeatureResolve::Native,
            vision: FeatureResolve::Native,
            pdf_documents: FeatureResolve::Native,
            thinking_visible: model.thinking_visible,
            context_limit: model.context_limit,
        }
    }

    async fn stream_turn(&self, request: TurnRequest) -> Result<ProviderStream, ProviderError> {
        self.stream_turn_ref(&request).await
    }

    async fn stream_prepared_turn_ref(
        &self,
        request: &TurnRequest,
        prepared: Option<crate::PreparedTurn>,
    ) -> Result<ProviderStream, ProviderError> {
        crate::scope_prepared_wire(prepared, self.stream_turn_ref(request)).await
    }
}

/// Bytes beyond the shared HTTP error-body ceiling are never parsed or logged.
pub(crate) async fn read_error_body_bounded(
    response: reqwest::Response,
) -> Result<Vec<u8>, ProviderError> {
    crate::read_http_error_body_bounded(response, "Anthropic").await
}

fn classify_http_body_read_error(
    status: u16,
    retry_after: Option<&str>,
    mut error: ProviderError,
) -> ProviderError {
    // Receiving an HTTP status completes transport classification. A later
    // diagnostic-body reset cannot turn the provider response into route loss.
    let classified = replay_anthropic_http_error(status, retry_after, &[]);
    error.kind = classified.kind;
    error.retryable = classified.retryable;
    error.retry_after_ms = classified.retry_after_ms;
    error.presentation = classified.presentation;
    error
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
    semantic_progress_timeout: Duration,
    native_computer: bool,
    context: crate::SseRequestContext,
) {
    stream_sse_source_with_native(
        response,
        account,
        sender,
        chunk_idle_timeout,
        semantic_progress_timeout,
        native_computer,
        context,
    )
    .await;
}

pub(crate) trait SseChunkSource {
    async fn next_chunk(
        &mut self,
        route_gating: crate::RouteGating,
    ) -> Result<Option<impl AsRef<[u8]> + Send + 'static>, ProviderError>;
}

impl SseChunkSource for reqwest::Response {
    async fn next_chunk(
        &mut self,
        route_gating: crate::RouteGating,
    ) -> Result<Option<impl AsRef<[u8]> + Send + 'static>, ProviderError> {
        self.chunk()
            .await
            .map_err(|error| transport_error_for_route(error, route_gating))
    }
}

#[cfg(test)]
pub(crate) async fn stream_sse_source<S: SseChunkSource>(
    source: S,
    account: Option<CredentialAlias>,
    sender: mpsc::Sender<ProviderStreamItem>,
    chunk_idle_timeout: Duration,
    semantic_progress_timeout: Duration,
    route_gating: crate::RouteGating,
) {
    stream_sse_source_with_native(
        source,
        account,
        sender,
        chunk_idle_timeout,
        semantic_progress_timeout,
        false,
        crate::SseRequestContext::capture(route_gating),
    )
    .await;
}

async fn stream_sse_source_with_native<S: SseChunkSource>(
    mut source: S,
    account: Option<CredentialAlias>,
    sender: mpsc::Sender<ProviderStreamItem>,
    chunk_idle_timeout: Duration,
    semantic_progress_timeout: Duration,
    native_computer: bool,
    context: crate::SseRequestContext,
) {
    let crate::SseRequestContext {
        route_gating,
        turn_trace,
    } = context;
    let mut decoder = SseDecoder::with_native_computer(account, native_computer);
    let mut progress = crate::ProviderProgressClock::new(
        chunk_idle_timeout,
        semantic_progress_timeout,
        route_gating,
    );
    loop {
        let chunk = match progress
            .wait_for_next(source.next_chunk(route_gating), &sender)
            .await
        {
            Ok(Some(Ok(Some(chunk)))) => chunk,
            Ok(Some(Ok(None))) => {
                send_items(&sender, decoder.finish()).await;
                return;
            }
            Ok(Some(Err(error))) => {
                let _ = sender.send(Err(error)).await;
                return;
            }
            Ok(None) => return,
            Err(crate::ProgressClockExpired::ChunkIdle) => {
                let _ = sender
                    .send(Err(stream_idle_error(chunk_idle_timeout)))
                    .await;
                return;
            }
            Err(crate::ProgressClockExpired::SemanticIdle) => {
                let _ = sender
                    .send(Err(crate::semantic_progress_timeout_error(
                        "Anthropic",
                        semantic_progress_timeout,
                    )))
                    .await;
                return;
            }
        };
        if let Some((trace, request_ordinal)) = &turn_trace {
            trace.emit_first_byte(*request_ordinal);
        }
        progress.observe_raw_chunk();
        let items = decoder.push(chunk.as_ref());
        if crate::has_semantic_progress(&items) {
            progress.observe_semantic_progress();
        }
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
    crate::reqwest_transport_error("Anthropic", error)
}

fn transport_error_for_route(
    error: reqwest::Error,
    route_gating: crate::RouteGating,
) -> ProviderError {
    crate::reqwest_transport_error_with_route_gating("Anthropic", error, route_gating)
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

fn anthropic_connect_timeout_error(timeout: Duration) -> ProviderError {
    let budget_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
    ProviderError::new(
        ProviderErrorKind::Transport,
        format!(
            "Anthropic connection preflight did not finish within its configured budget; opened_within_ms={budget_ms} budget_ms={budget_ms}"
        ),
    )
    .with_timeout_budget(budget_ms, budget_ms)
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

/// Replays captured native-computer SSE through the live translation seam.
/// This is fixture/test support; ordinary replay remains byte-identical and
/// generic unless the request actually advertised Anthropic's native tool.
#[must_use]
pub fn replay_anthropic_native_computer_sse(bytes: &[u8]) -> Vec<ProviderStreamItem> {
    let mut decoder = SseDecoder::with_native_computer(None, true);
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
    let billing_exhausted = parsed.as_ref().is_some_and(|envelope| {
        anthropic_billing_exhausted(&envelope.error.kind, &envelope.error.message)
    });
    let hosted_web_rejected = parsed.as_ref().is_some_and(|envelope| {
        let message = envelope.error.message.to_ascii_lowercase();
        (message.contains("web_search") || message.contains("web_fetch"))
            && [
                "unsupported",
                "unavailable",
                "not allowed",
                "not enabled",
                "rejected",
            ]
            .iter()
            .any(|needle| message.contains(needle))
    });
    let kind = match status {
        _ if billing_exhausted => ProviderErrorKind::QuotaExhausted,
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
    let message = if kind == ProviderErrorKind::QuotaExhausted {
        "provider quota/credit exhausted — retrying will not help; check billing or switch account"
            .to_owned()
    } else {
        format!(
            "Anthropic HTTP {status} returned {}",
            provider_kind_name(kind)
        )
    };
    let retry_after_ms = matches!(
        kind,
        ProviderErrorKind::RateLimited
            | ProviderErrorKind::Overloaded
            | ProviderErrorKind::Transport
    )
    .then(|| crate::parse_retry_after_ms(retry_after))
    .flatten();
    let error = if hosted_web_rejected {
        ProviderError::new(kind, message).with_presentation(ErrorPresentation::new(
            "provider-web-tool-rejected",
            "Provider web tool unavailable",
            "Anthropic explicitly rejected its hosted web tool; Haider can retry this turn with the local web_fetch equivalent.",
            ErrorScope::Tool,
            [ErrorAction::Retry],
        ))
    } else {
        match body_kind {
            Some("account_not_found_error" | "account_deleted_error") => {
                ProviderError::new_with_presentation(
                    kind,
                    message,
                    crate::account_deleted_presentation(),
                )
            }
            Some("account_deactivated_error" | "account_revoked_error") => {
                ProviderError::new_with_presentation(
                    kind,
                    message,
                    crate::account_revoked_presentation(),
                )
            }
            _ => ProviderError::new(kind, message),
        }
    };
    error
        .with_retry_after_ms(retry_after_ms)
        .with_http_metadata(status, None)
}

pub(crate) fn anthropic_billing_exhausted(kind: &str, message: &str) -> bool {
    let kind = kind.to_ascii_lowercase();
    let message = message.to_ascii_lowercase();
    kind.contains("billing")
        || kind.contains("credit")
        || message.contains("billing")
        || message.contains("credit balance")
        || message.contains("payment required")
}

#[derive(Debug, Deserialize)]
struct ErrorEnvelope {
    error: WireApiError,
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod oauth_cache_tests {
    use haider_accounts::{MemoryVault, Vault};
    use haider_protocol::ids::CredentialAlias;
    use reqwest::header::AUTHORIZATION;

    use super::*;
    use crate::{Message, PromptCacheMetadata, TurnRequest};

    fn credential(alias: &str) -> SecretHandle {
        let vault = MemoryVault::new();
        let alias = CredentialAlias::new(alias);
        vault
            .put(&alias, b"anthropic-cache-test-secret")
            .expect("store cache-test secret");
        vault.resolve(&alias).expect("resolve cache-test secret")
    }

    fn cache_turn(provider: &str) -> TurnRequest {
        TurnRequest {
            messages: vec![
                Message::user_text("stable prefix"),
                Message::user_text("current turn"),
            ],
            model: "claude-haiku-4-5-20251001".into(),
            max_tokens: 16,
            system_prompt: Some("stable system".into()),
            tools: Vec::new(),
            attachments: Vec::new(),
            cache_metadata: Some(PromptCacheMetadata {
                stable_history_end: 1,
                current_user_start: 1,
                provider: provider.into(),
                account_scope: Some("account-a".into()),
                expected_later_reads: 2,
                reuse_gap_ms: Some(30_000),
                stable_prefix_tokens: 4_096,
                ..PromptCacheMetadata::default()
            }),
        }
    }

    /// HAIDERANTHCACHE regression: a real subscription model enables caching
    /// without the old manual verification seam, uses Claude Code's 1h TTL,
    /// and composes the cache betas. The API-key request retains its existing
    /// 5m selector and does not gain OAuth/cache beta headers.
    #[tokio::test]
    async fn oauth_real_model_uses_one_hour_cache_betas_without_changing_api_key() {
        let oauth = AnthropicProvider::new_with_auth(
            credential("oauth-cache"),
            "claude-haiku-4-5-20251001",
            AnthropicAuthMode::OAuthBearer,
            None,
        )
        .expect("OAuth provider");
        assert!(
            !oauth.prompt_caching_verified,
            "the real-model policy, not a test override, enables this path"
        );
        let oauth_payload = oauth
            .request_payload(&cache_turn(ANTHROPIC_OAUTH_PROVIDER_NAME))
            .expect("OAuth cache payload");
        let one_hour = serde_json::json!({"type": "ephemeral", "ttl": "1h"});
        assert_eq!(oauth_payload["system"][1]["cache_control"], one_hour);
        assert_eq!(
            oauth_payload["messages"][0]["content"][0]["cache_control"], one_hour,
            "the stable boundary lands on its final content block"
        );
        assert!(
            oauth_payload["messages"][1]["content"][0]
                .get("cache_control")
                .is_none(),
            "the volatile current turn stays beyond the stable breakpoint"
        );
        let oauth_request = oauth
            .request_body(oauth_payload)
            .await
            .expect("OAuth HTTP request");
        assert_eq!(
            oauth_request
                .headers()
                .get(ANTHROPIC_OAUTH_BETA_HEADER)
                .expect("OAuth cache betas"),
            "oauth-2025-04-20,prompt-caching-scope-2026-01-05,extended-cache-ttl-2025-04-11"
        );
        assert!(oauth_request.headers().contains_key(AUTHORIZATION));
        assert!(!oauth_request.headers().contains_key("x-api-key"));

        let api_key = AnthropicProvider::new(credential("api-cache"), "claude-haiku-4-5-20251001")
            .expect("API-key provider");
        let api_payload = api_key
            .request_payload(&cache_turn(ANTHROPIC_PROVIDER_NAME))
            .expect("API-key cache payload");
        assert_eq!(
            api_payload["system"][0]["cache_control"],
            serde_json::json!({"type": "ephemeral", "ttl": "5m"}),
            "the API-key TTL policy is unchanged"
        );
        let api_request = api_key
            .request_body(api_payload)
            .await
            .expect("API-key HTTP request");
        assert!(api_request.headers().contains_key("x-api-key"));
        assert!(!api_request.headers().contains_key(AUTHORIZATION));
        assert!(
            !api_request
                .headers()
                .contains_key(ANTHROPIC_OAUTH_BETA_HEADER),
            "the API-key cache path gains no OAuth-only beta header"
        );
    }
}
