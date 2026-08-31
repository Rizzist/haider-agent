//! Google Gemini GenerateContent API adapter.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use haider_accounts::SecretHandle;
use haider_protocol::ids::{ArtifactRef, CredentialAlias};
use haider_protocol::provider::{
    Block, CacheStatAvailability, CapabilityDoc, FeatureResolve, FinishReason, NormalizedUsage,
    ReasoningAccounting, StreamEvent, Usage, UsageSource, WebSource,
};
use haider_protocol::tool::AttachmentBlock;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderValue, RETRY_AFTER};
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Serialize, Serializer};
use tokio::sync::{Mutex, mpsc};

use crate::origin::{FixedDnsResolver, FixedOriginGuard, SystemFixedDnsResolver};
use crate::{
    MessageRole, Provider, ProviderError, ProviderErrorKind, ProviderStream, ProviderStreamItem,
    TurnRequest, Utf8Assembler,
};

pub const GEMINI_PROVIDER_NAME: &str = "gemini";
pub const GEMINI_API_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
pub const GEMINI_MODELS_URL: &str = "https://generativelanguage.googleapis.com/v1beta/models";
pub const GEMINI_CACHED_CONTENTS_URL: &str =
    "https://generativelanguage.googleapis.com/v1beta/cachedContents";
const GEMINI_API_HOST: &str = "generativelanguage.googleapis.com";
const STREAM_CAPACITY: usize = 32;
const OPAQUE_KIND: &str = "signed_part";
const CALL_ID_PREFIX: &str = "gemini-call-";
const CLIENT_BUILD_METRIC_CAPACITY: usize = 64;
const TRANSPORT_CONFIG: GeminiTransportConfig = GeminiTransportConfig {
    retry_policy: GeminiRetryPolicy::Never,
    connect_timeout: Duration::from_secs(10),
    response_open_timeout: Duration::from_secs(30),
    chunk_idle_timeout: Duration::from_secs(90),
    semantic_progress_timeout: Duration::from_secs(5 * 60),
};

struct GeminiTransport {
    client: reqwest::Client,
    fixed_origin_guard: Arc<FixedOriginGuard>,
}

static GEMINI_CLIENT_BUILD_COUNT: AtomicUsize = AtomicUsize::new(0);
static GEMINI_CLIENT_BUILDS_BY_ENDPOINT: OnceLock<StdMutex<VecDeque<(String, usize)>>> =
    OnceLock::new();

fn build_gemini_transport(
    api_url: &str,
    resolver: Arc<dyn FixedDnsResolver>,
) -> Result<GeminiTransport, ProviderError> {
    GEMINI_CLIENT_BUILD_COUNT.fetch_add(1, Ordering::Relaxed);
    let fixed_origin_guard = Arc::new(FixedOriginGuard::new_allowing(
        &[api_url, GEMINI_CACHED_CONTENTS_URL],
        GEMINI_API_HOST,
        resolver,
    )?);
    let transport = GeminiProvider::transport_config();
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(match transport.retry_policy {
            GeminiRetryPolicy::Never => reqwest::retry::never(),
        })
        .pool_idle_timeout(crate::PROVIDER_POOL_IDLE_TIMEOUT)
        .http2_adaptive_window(true)
        .connect_timeout(transport.connect_timeout)
        .dns_resolver(Arc::clone(&fixed_origin_guard))
        .build()
        .map_err(|error| {
            ProviderError::new(
                ProviderErrorKind::Internal,
                format!("could not construct Gemini HTTP client: {error}"),
            )
        })?;
    record_gemini_client_build(api_url);
    Ok(GeminiTransport {
        client,
        fixed_origin_guard,
    })
}

fn record_gemini_client_build(api_url: &str) {
    let builds = GEMINI_CLIENT_BUILDS_BY_ENDPOINT
        .get_or_init(|| StdMutex::new(VecDeque::with_capacity(CLIENT_BUILD_METRIC_CAPACITY)));
    let Ok(mut builds) = builds.lock() else {
        return;
    };
    if let Some(index) = builds.iter().position(|(endpoint, _)| endpoint == api_url)
        && let Some((endpoint, count)) = builds.remove(index)
    {
        builds.push_back((endpoint, count.saturating_add(1)));
        return;
    }
    if builds.len() >= CLIENT_BUILD_METRIC_CAPACITY {
        builds.pop_front();
    }
    builds.push_back((api_url.to_owned(), 1));
}

/// Process-lifetime construction counter used by performance regression
/// harnesses. Production account resolution reuses the complete adapter;
/// standalone constructors deliberately own a configuration-local client.
#[doc(hidden)]
#[must_use]
pub fn gemini_http_client_build_count() -> usize {
    GEMINI_CLIENT_BUILD_COUNT.load(Ordering::Relaxed)
}

/// Exact-endpoint construction count for performance harnesses. Metric
/// retention is LRU-bounded independently from adapter/client retention.
#[doc(hidden)]
#[must_use]
pub fn gemini_model_http_client_build_count(model: &str) -> usize {
    let Ok(api_url) = gemini_stream_endpoint(model) else {
        return 0;
    };
    GEMINI_CLIENT_BUILDS_BY_ENDPOINT
        .get_or_init(|| StdMutex::new(VecDeque::with_capacity(CLIENT_BUILD_METRIC_CAPACITY)))
        .lock()
        .ok()
        .and_then(|builds| {
            builds
                .iter()
                .find_map(|(endpoint, count)| (endpoint == &api_url).then_some(*count))
        })
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeminiRetryPolicy {
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GeminiTransportConfig {
    pub retry_policy: GeminiRetryPolicy,
    pub connect_timeout: Duration,
    pub response_open_timeout: Duration,
    pub chunk_idle_timeout: Duration,
    pub semantic_progress_timeout: Duration,
}

/// Raw response returned only to the explicitly gated fixture-promotion path.
#[derive(Debug)]
pub struct GeminiCapture {
    pub status: u16,
    pub retry_after: Option<String>,
    pub body: Vec<u8>,
}

/// Gemini adapter backed by one already-resolved API key.
#[derive(Debug)]
pub struct GeminiProvider {
    client: reqwest::Client,
    credential: Arc<SecretHandle>,
    account: Option<CredentialAlias>,
    model: String,
    api_url: String,
    fixed_origin_guard: Arc<FixedOriginGuard>,
    /// Session-selected effort (G3), injected as
    /// `generationConfig.thinkingConfig.thinkingLevel` for 3.x-named models
    /// whose pinned static ladder declares the value. Never `thinkingBudget`
    /// (the 2.5-era numeric knob is deliberately unmodeled) and never both.
    effort: Option<String>,
    /// W-B: declare the `google_search` + `url_context` built-ins. The
    /// request builder name-gates this to 3.x models (the G3 pattern) —
    /// 2.5-era models cannot combine built-ins with function declarations.
    web_builtins: bool,
    cache_registry: Option<Arc<GeminiCacheRegistry>>,
    cache_backend: Arc<dyn GeminiCacheBackend>,
}

#[async_trait]
pub trait GeminiCacheBackend: std::fmt::Debug + Send + Sync {
    async fn create_cached_content(
        &self,
        payload: &serde_json::Value,
    ) -> Result<String, ProviderError>;
    async fn delete_cached_content(&self, name: &str) -> Result<(), ProviderError>;
}

#[derive(Debug, Default)]
pub struct GeminiCacheRegistry {
    resources: Mutex<HashMap<String, GeminiCachedResource>>,
}

#[derive(Debug)]
struct GeminiCachedResource {
    epoch: String,
    name: String,
    content_blocks: Vec<haider_protocol::cache::ProviderViewBlockRefV1>,
    stable_prefix_tokens: u64,
    expires_at: tokio::time::Instant,
    backend: Arc<dyn GeminiCacheBackend>,
}

#[derive(Debug)]
struct GeminiHttpCacheBackend {
    client: reqwest::Client,
    credential: Arc<SecretHandle>,
    fixed_origin_guard: Arc<FixedOriginGuard>,
}

impl GeminiProvider {
    pub fn new(credential: SecretHandle, model: impl Into<String>) -> Result<Self, ProviderError> {
        let model = model.into();
        let api_url = gemini_stream_endpoint(&model)?;
        let transport = build_gemini_transport(&api_url, Arc::new(SystemFixedDnsResolver))?;
        Ok(Self::with_transport(credential, model, api_url, transport))
    }

    #[cfg(test)]
    pub(crate) fn new_with_dns_resolver(
        credential: SecretHandle,
        model: impl Into<String>,
        resolver: Arc<dyn FixedDnsResolver>,
    ) -> Result<Self, ProviderError> {
        let model = model.into();
        let api_url = gemini_stream_endpoint(&model)?;
        let transport = build_gemini_transport(&api_url, resolver)?;
        Ok(Self::with_transport(credential, model, api_url, transport))
    }

    fn with_transport(
        credential: SecretHandle,
        model: String,
        api_url: String,
        transport: GeminiTransport,
    ) -> Self {
        let GeminiTransport {
            client,
            fixed_origin_guard,
        } = transport;
        let credential = Arc::new(credential);
        let cache_backend = Arc::new(GeminiHttpCacheBackend {
            client: client.clone(),
            credential: Arc::clone(&credential),
            fixed_origin_guard: Arc::clone(&fixed_origin_guard),
        });
        Self {
            client,
            credential,
            account: None,
            model,
            api_url,
            fixed_origin_guard,
            effort: None,
            web_builtins: false,
            cache_registry: None,
            cache_backend,
        }
    }

    #[must_use]
    pub const fn transport_config() -> GeminiTransportConfig {
        TRANSPORT_CONFIG
    }

    #[must_use]
    pub fn with_account(mut self, account: CredentialAlias) -> Self {
        self.account = Some(account);
        self
    }

    /// Sets the session-selected effort injected as `thinkingLevel`.
    #[must_use]
    pub fn with_effort(mut self, effort: Option<String>) -> Self {
        self.effort = effort;
        self
    }

    /// Declares the `google_search` + `url_context` built-ins (W-B). The
    /// request builder still name-gates the declaration to 3.x models.
    #[must_use]
    pub fn with_web_builtins(mut self, web_builtins: bool) -> Self {
        self.web_builtins = web_builtins;
        self
    }

    /// Installs the daemon-owned ephemeral resource registry shared by all
    /// Gemini provider instances in one runtime.
    #[must_use]
    pub fn with_cache_registry(mut self, registry: Arc<GeminiCacheRegistry>) -> Self {
        self.cache_registry = Some(registry);
        self
    }

    #[cfg(test)]
    pub(crate) fn client_debug(&self) -> String {
        format!("{:?}", self.client)
    }

    #[cfg(test)]
    pub(crate) fn stall_fixed_connection_resolution(&self) {
        self.fixed_origin_guard.stall_connection_resolution();
    }

    #[cfg(test)]
    pub(crate) fn fixed_connection_resolution_count(&self) -> usize {
        self.fixed_origin_guard.connection_resolution_count()
    }

    #[cfg(test)]
    pub(crate) async fn execute_request_for_test(
        &self,
        request: reqwest::Request,
    ) -> Result<reqwest::Response, reqwest::Error> {
        self.client.execute(request).await
    }

    pub fn request_payload(
        &self,
        request: &TurnRequest,
    ) -> Result<serde_json::Value, ProviderError> {
        self.validate_model(request)?;
        gemini_request_json(request, self.effort.as_deref(), self.web_builtins)
    }

    pub async fn capture_response(
        &self,
        request: &TurnRequest,
    ) -> Result<GeminiCapture, ProviderError> {
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
        Ok(GeminiCapture {
            status,
            retry_after,
            body,
        })
    }

    fn validate_model(&self, request: &TurnRequest) -> Result<(), ProviderError> {
        if request.model == self.model {
            Ok(())
        } else {
            Err(invalid_request(format!(
                "Gemini provider selected model `{}`, but turn requested `{}`",
                self.model, request.model
            )))
        }
    }

    fn api_key_header(&self) -> Result<HeaderValue, ProviderError> {
        let mut value = HeaderValue::from_bytes(self.credential.expose_secret()).map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::Authentication,
                "resolved Gemini credential is not a valid HTTP header value",
            )
        })?;
        value.set_sensitive(true);
        Ok(value)
    }

    pub(crate) async fn request_body(
        &self,
        payload: serde_json::Value,
    ) -> Result<reqwest::Request, ProviderError> {
        let request = self.request_builder().await?;
        let body = crate::serialize_json_body(payload)?;
        request.body(body).build().map_err(transport_error)
    }

    async fn request_builder(&self) -> Result<reqwest::RequestBuilder, ProviderError> {
        // The guard validates and resolves before credential material is
        // placed on a request. `alt=sse` is a fixed non-routing query added
        // only after the exact HTTPS path has passed the gate.
        self.fixed_origin_guard
            .validate_endpoint(&self.api_url)
            .await?;
        let request_url = format!("{}?alt=sse", self.api_url);
        Ok(self
            .client
            .post(request_url)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "text/event-stream")
            .header("x-goog-api-key", self.api_key_header()?))
    }

    async fn send_request(
        &self,
        request: &TurnRequest,
    ) -> Result<reqwest::Response, ProviderError> {
        let (full_payload, history_boundary) = match crate::take_prepared_wire_payload() {
            Some(prepared) => (prepared.payload, prepared.history_boundary),
            None => {
                self.validate_model(request)?;
                let boundary = request
                    .cache_metadata
                    .as_ref()
                    .map_or(request.messages.len(), |metadata| {
                        metadata.cacheable_history_end()
                    });
                let (payload, boundary, _) = gemini_request_json_with_boundary(
                    request,
                    &request.tools,
                    self.effort.as_deref(),
                    self.web_builtins,
                    boundary,
                )?;
                (payload, Some(boundary))
            }
        };
        let payload = if let Some(registry) = &self.cache_registry {
            registry
                .prepare_generate_payload_with_boundary(
                    request,
                    full_payload,
                    history_boundary,
                    Arc::clone(&self.cache_backend),
                    self.web_builtins
                        && crate::effort::gemini_web_builtins_supported(&request.model),
                )
                .await
        } else {
            full_payload
        };
        let request = self.request_body(payload).await?;
        crate::route_gated_timeout(
            Self::transport_config().response_open_timeout,
            self.client.execute(request),
            crate::RouteGating::Enabled,
        )
        .await
        .map_err(|_| response_open_timeout_error(Self::transport_config().response_open_timeout))?
        .map_err(transport_error)
    }

    async fn stream_turn_ref(
        &self,
        request: &TurnRequest,
    ) -> Result<ProviderStream, ProviderError> {
        let next_call_index = next_synthesized_call_index(request)?;
        let response = self.send_request(request).await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let request_id = response
                .headers()
                .get("x-request-id")
                .or_else(|| response.headers().get("x-goog-request-id"))
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let retry_after = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let error = match read_error_body_bounded(response).await {
                Ok(body) => replay_gemini_http_error(status, retry_after.as_deref(), &body),
                Err(error) => classify_http_body_read_error(status, retry_after.as_deref(), error),
            };
            return Err(error.with_http_metadata(status, request_id.as_deref()));
        }

        let (sender, receiver) = mpsc::channel(STREAM_CAPACITY);
        let account = self.account.clone();
        let chunk_idle_timeout = Self::transport_config().chunk_idle_timeout;
        let semantic_progress_timeout = Self::transport_config().semantic_progress_timeout;
        let producer = tokio::spawn(async move {
            stream_sse_source(
                response,
                account,
                next_call_index,
                sender,
                chunk_idle_timeout,
                semantic_progress_timeout,
            )
            .await;
        });
        Ok(ProviderStream::owned(receiver, producer))
    }
}

impl GeminiProvider {
    fn prepare_turn_with_tools_inner(
        &self,
        request: &TurnRequest,
        tools: &[crate::ToolDefinition],
        attachment_moves: Option<&mut [crate::StagedAttachmentMove]>,
    ) -> Option<crate::PreparedTurn> {
        let boundary = request.cache_metadata.as_ref()?.cacheable_history_end();
        self.validate_model(request).ok()?;
        let (rendered_payload, history_boundary, previous_history_boundary) =
            gemini_request_json_with_boundary(
                request,
                tools,
                self.effort.as_deref(),
                self.web_builtins,
                boundary,
            )
            .ok()?;
        let full_payload = crate::AttachmentMovePayload::new(rendered_payload, attachment_moves);
        let contents = payload_contents(&full_payload)?;
        let history_blocks = gemini_provider_view_blocks(contents, history_boundary)?;
        let (previous_history_blocks, previous_history_block_len) = match previous_history_boundary
        {
            Some(boundary) => {
                let (blocks, reusable_len) = gemini_previous_provider_view_block_refs(
                    contents,
                    history_boundary,
                    &history_blocks,
                    boundary,
                )?;
                (Some(blocks), reusable_len)
            }
            None => (None, None),
        };
        let metadata = request.cache_metadata.as_ref()?;
        let boundaries = (metadata.cacheable_history_end() > 0)
            .then(|| haider_protocol::cache::ProviderViewBoundaryV1 {
                section: "history".into(),
                message_end: Some(
                    u64::try_from(metadata.cacheable_history_end()).unwrap_or(u64::MAX),
                ),
            })
            .into_iter()
            .collect();
        let mut provider_view = crate::cachemaxxing::prepared_serialized_provider_view(
            request,
            "gemini_generate_content",
            full_payload.get("system_instruction"),
            full_payload.get("tools"),
            history_blocks,
            previous_history_blocks,
            boundaries,
        )?;
        let (prefix_digests, block_previous_digest, provider_view_storage_blobs) =
            crate::rendered_prefix_digests_from_provider_view(
                request,
                &mut provider_view,
                false,
                previous_history_block_len,
            )?;
        let previous_immutable_history_digest = match previous_history_boundary {
            Some(previous) => {
                block_previous_digest.or_else(|| gemini_history_digest(&full_payload, previous))
            }
            None => None,
        };
        Some(crate::PreparedTurn {
            prefix_digests,
            previous_immutable_history_digest,
            cache_control: haider_protocol::provider::CacheControlObservationV1::Unavailable,
            provider_view: Some(provider_view),
            provider_view_storage_blobs,
            wire: Some(crate::PreparedWire {
                payload: full_payload.commit(),
                history_boundary: Some(history_boundary),
            }),
        })
    }
}

#[async_trait]
impl Provider for GeminiProvider {
    fn trusts_default_route_absence(&self) -> bool {
        true
    }

    fn credential_surface(&self) -> crate::ProviderCredentialSurface {
        crate::ProviderCredentialSurface::ApiKey
    }

    fn usage_lane_dimensions(&self) -> haider_protocol::provider::UsageLaneDimensions {
        haider_protocol::provider::UsageLaneDimensions {
            api_family: Some("gemini_generate_content".into()),
            effort: self.effort.clone(),
            speed: None,
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

    async fn prewarm(&self) {
        crate::optional_http_prewarm(&self.client, &self.api_url).await;
    }

    async fn capabilities(&self) -> CapabilityDoc {
        let context_limit = gemini_context_limit(&self.model);
        CapabilityDoc {
            provider: GEMINI_PROVIDER_NAME.into(),
            parallel_tools: FeatureResolve::Native,
            // GenerateContent returns each function-call argument object as a
            // whole; the adapter normalizes it to one complete args delta.
            streaming_tool_args: FeatureResolve::ExplicitlyEmulated,
            vision: FeatureResolve::Native,
            pdf_documents: FeatureResolve::ExplicitlyEmulated,
            thinking_visible: if self.model.starts_with("gemini-2.5")
                || self.model.starts_with("gemini-3")
            {
                FeatureResolve::Native
            } else {
                FeatureResolve::Unsupported
            },
            context_limit,
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

#[async_trait]
impl GeminiCacheBackend for GeminiHttpCacheBackend {
    async fn create_cached_content(
        &self,
        payload: &serde_json::Value,
    ) -> Result<String, ProviderError> {
        self.fixed_origin_guard
            .validate_endpoint(GEMINI_CACHED_CONTENTS_URL)
            .await?;
        let mut api_key =
            HeaderValue::from_bytes(self.credential.expose_secret()).map_err(|_| {
                ProviderError::new(
                    ProviderErrorKind::Authentication,
                    "resolved Gemini credential is not a valid HTTP header value",
                )
            })?;
        api_key.set_sensitive(true);
        let request = self
            .client
            .post(GEMINI_CACHED_CONTENTS_URL)
            .header(CONTENT_TYPE, "application/json")
            .header("x-goog-api-key", api_key)
            .json(payload)
            .build()
            .map_err(transport_error)?;
        let response = crate::route_gated_timeout(
            GeminiProvider::transport_config().response_open_timeout,
            self.client.execute(request),
            crate::RouteGating::Enabled,
        )
        .await
        .map_err(|_| {
            response_open_timeout_error(GeminiProvider::transport_config().response_open_timeout)
        })?
        .map_err(transport_error)?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let retry_after = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let request_id = response
                .headers()
                .get("x-request-id")
                .or_else(|| response.headers().get("x-goog-request-id"))
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let error = match read_error_body_bounded(response).await {
                Ok(body) => replay_gemini_http_error(status, retry_after.as_deref(), &body),
                Err(error) => classify_http_body_read_error(status, retry_after.as_deref(), error),
            };
            return Err(error.with_http_metadata(status, request_id.as_deref()));
        }
        let body = response.bytes().await.map_err(transport_error)?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|_| malformed("Gemini CachedContent create returned malformed JSON"))?;
        let name = value
            .get("name")
            .and_then(serde_json::Value::as_str)
            .filter(|name| valid_gemini_cache_name(name))
            .ok_or_else(|| malformed("Gemini CachedContent create returned an invalid name"))?;
        Ok(name.to_owned())
    }

    async fn delete_cached_content(&self, name: &str) -> Result<(), ProviderError> {
        if !valid_gemini_cache_name(name) {
            return Err(invalid_request("Gemini CachedContent name is invalid"));
        }
        let endpoint = format!("{GEMINI_API_BASE_URL}/{name}");
        self.fixed_origin_guard
            .validate_trusted_origin_endpoint(&endpoint)
            .await?;
        let mut api_key =
            HeaderValue::from_bytes(self.credential.expose_secret()).map_err(|_| {
                ProviderError::new(
                    ProviderErrorKind::Authentication,
                    "resolved Gemini credential is not a valid HTTP header value",
                )
            })?;
        api_key.set_sensitive(true);
        let request = self
            .client
            .delete(endpoint)
            .header("x-goog-api-key", api_key)
            .build()
            .map_err(transport_error)?;
        let response = crate::route_gated_timeout(
            GeminiProvider::transport_config().response_open_timeout,
            self.client.execute(request),
            crate::RouteGating::Enabled,
        )
        .await
        .map_err(|_| {
            response_open_timeout_error(GeminiProvider::transport_config().response_open_timeout)
        })?
        .map_err(transport_error)?;
        if response.status().is_success() || response.status().as_u16() == 404 {
            Ok(())
        } else {
            let status = response.status().as_u16();
            let retry_after = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok());
            let request_id = response
                .headers()
                .get("x-request-id")
                .or_else(|| response.headers().get("x-goog-request-id"))
                .and_then(|value| value.to_str().ok());
            Err(replay_gemini_http_error(status, retry_after, &[])
                .with_http_metadata(status, request_id))
        }
    }
}

impl GeminiCacheRegistry {
    /// Deletes the resource owned by one session scope. A failed delete is
    /// retained so a later switch-away/transition can retry it.
    pub async fn delete_scope(&self, scope: &str) -> Result<(), ProviderError> {
        let existing = { self.resources.lock().await.remove(scope) };
        let Some(existing) = existing else {
            return Ok(());
        };
        if let Err(error) = existing.backend.delete_cached_content(&existing.name).await {
            self.resources
                .lock()
                .await
                .insert(scope.to_owned(), existing);
            return Err(error);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn prepare_generate_payload(
        &self,
        request: &TurnRequest,
        full_payload: serde_json::Value,
        backend: Arc<dyn GeminiCacheBackend>,
        effort: Option<&str>,
        web_builtins: bool,
    ) -> serde_json::Value {
        let history_boundary = gemini_request_json_with_boundary(
            request,
            &request.tools,
            effort,
            web_builtins,
            request
                .cache_metadata
                .as_ref()
                .map_or(request.messages.len(), |metadata| {
                    metadata.cacheable_history_end()
                }),
        )
        .ok()
        .map(|(_, boundary, _)| boundary);
        self.prepare_generate_payload_with_boundary(
            request,
            full_payload,
            history_boundary,
            backend,
            web_builtins,
        )
        .await
    }

    pub(crate) async fn prepare_generate_payload_with_boundary(
        &self,
        request: &TurnRequest,
        full_payload: serde_json::Value,
        history_boundary: Option<crate::PreparedHistoryBoundary>,
        backend: Arc<dyn GeminiCacheBackend>,
        web_builtins: bool,
    ) -> serde_json::Value {
        let Some(metadata) = request.cache_metadata.as_ref().filter(|metadata| {
            metadata.boundaries_valid(request.messages.len())
                && metadata.provider == GEMINI_PROVIDER_NAME
                && metadata.account_scope.is_some()
        }) else {
            return full_payload;
        };
        let scope = metadata.session_scope.clone();
        let existing = { self.resources.lock().await.remove(&scope) };
        if let Some(existing) = existing {
            let expired = existing.expires_at <= tokio::time::Instant::now();
            if existing.epoch == metadata.cache_epoch
                && !expired
                && payload_contents(&full_payload)
                    .and_then(|contents| {
                        contents
                            .get(..existing.content_blocks.len())
                            .and_then(gemini_content_block_refs)
                    })
                    .is_some_and(|blocks| blocks == existing.content_blocks)
                && !gemini_cached_coverage_needs_refresh(
                    existing.stable_prefix_tokens,
                    metadata.stable_prefix_tokens,
                )
            {
                let payload = gemini_cached_generate_payload(
                    full_payload,
                    &existing.name,
                    existing.content_blocks.len(),
                );
                self.resources.lock().await.insert(scope, existing);
                return payload;
            }
            if existing
                .backend
                .delete_cached_content(&existing.name)
                .await
                .is_err()
            {
                // Never create a second paid resource while the superseded
                // one could not be deleted. Retain it for a later retry and
                // send the exact implicit full-history request now.
                // An entry beyond Haider's own matching TTL is already dead
                // provider-side; discard that local handle so an unavailable
                // DELETE endpoint cannot permanently disable explicit reuse.
                if !expired {
                    self.resources.lock().await.insert(scope, existing);
                }
                return full_payload;
            }
        }

        let Some(minimum) = crate::cacheable_prompt_minimum(GEMINI_PROVIDER_NAME, &request.model)
        else {
            return full_payload;
        };
        if metadata.stable_prefix_tokens < minimum
            || metadata.expected_later_reads < 2
            || web_builtins
            || metadata.cacheable_history_end() == 0
            || metadata.cacheable_history_end() > request.messages.len()
        {
            return full_payload;
        }

        let Some(history_boundary) = history_boundary else {
            return full_payload;
        };
        let Some(stable_contents) = gemini_cacheable_contents(&full_payload, history_boundary)
        else {
            return full_payload;
        };
        if stable_contents.is_empty() {
            // Adjacent equal Gemini roles can coalesce across a split. Such a
            // boundary is not byte-stable and stays on implicit caching.
            return full_payload;
        }
        let Some(content_blocks) = gemini_content_block_refs(stable_contents) else {
            return full_payload;
        };
        let create_payload =
            gemini_cached_content_create_payload(request, &full_payload, stable_contents);
        let Ok(name) = backend.create_cached_content(&create_payload).await else {
            return full_payload;
        };
        drop(create_payload);
        let payload = gemini_cached_generate_payload(full_payload, &name, content_blocks.len());
        self.resources.lock().await.insert(
            scope,
            GeminiCachedResource {
                epoch: metadata.cache_epoch.clone(),
                name,
                content_blocks,
                stable_prefix_tokens: metadata.stable_prefix_tokens,
                expires_at: tokio::time::Instant::now() + Duration::from_secs(3_600),
                backend,
            },
        );
        payload
    }
}

fn gemini_cached_coverage_needs_refresh(cached_tokens: u64, current_tokens: u64) -> bool {
    // Refresh below 80% coverage. The multiplicative threshold makes writes
    // geometric as a conversation grows instead of recreating on every turn.
    u128::from(cached_tokens) * 100 < u128::from(current_tokens) * 80
}

fn payload_contents(payload: &serde_json::Value) -> Option<&[serde_json::Value]> {
    payload.get("contents")?.as_array().map(Vec::as_slice)
}

fn gemini_cached_content_create_payload(
    request: &TurnRequest,
    full_payload: &serde_json::Value,
    stable_contents: &[serde_json::Value],
) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "model": format!("models/{}", request.model),
        "contents": stable_contents,
        "ttl": "3600s",
    });
    // The literal above is always an object; a non-object would simply skip
    // the optional inserts rather than panic.
    if let Some(object) = payload.as_object_mut() {
        if let Some(system) = full_payload.get("system_instruction") {
            object.insert("systemInstruction".into(), system.clone());
        }
        if let Some(tools) = full_payload.get("tools") {
            object.insert("tools".into(), tools.clone());
        }
    }
    payload
}

fn gemini_cached_generate_payload(
    mut full_payload: serde_json::Value,
    name: &str,
    covered_contents: usize,
) -> serde_json::Value {
    let Some(object) = full_payload.as_object_mut() else {
        return full_payload;
    };
    object.remove("system_instruction");
    object.remove("tools");
    if let Some(contents) = object
        .get_mut("contents")
        .and_then(serde_json::Value::as_array_mut)
    {
        contents.drain(..covered_contents.min(contents.len()));
    }
    object.insert("cachedContent".into(), serde_json::json!(name));
    full_payload
}

fn valid_gemini_cache_name(name: &str) -> bool {
    let Some(identifier) = name.strip_prefix("cachedContents/") else {
        return false;
    };
    !identifier.is_empty()
        && identifier
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn gemini_stream_endpoint(model: &str) -> Result<String, ProviderError> {
    if model.is_empty()
        || !model
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(invalid_request(
            "Gemini model must be a nonempty model slug without path or query syntax",
        ));
    }
    let mut url = reqwest::Url::parse(GEMINI_API_BASE_URL)
        .map_err(|_| internal("Gemini API base URL is invalid"))?;
    url.path_segments_mut()
        .map_err(|_| internal("Gemini API base URL cannot carry path segments"))?
        .pop_if_empty()
        .push("models")
        .push(&format!("{model}:streamGenerateContent"));
    Ok(url.into())
}

fn gemini_context_limit(model: &str) -> u64 {
    if model.starts_with("gemini-3")
        || model.starts_with("gemini-2.5")
        || model.starts_with("gemini-2.0")
        || model.starts_with("gemini-1.5")
    {
        1_048_576
    } else {
        128_000
    }
}

fn next_synthesized_call_index(request: &TurnRequest) -> Result<u64, ProviderError> {
    let mut count = 0_u64;
    let mut greatest = None;
    for message in &request.messages {
        for block in &message.blocks {
            if let Block::ToolCall { call_id, .. } = block {
                count = count.saturating_add(1);
                if let Some(suffix) = call_id.strip_prefix(CALL_ID_PREFIX)
                    && let Ok(index) = u64::from_str_radix(suffix, 16)
                {
                    greatest = Some(greatest.map_or(index, |current: u64| current.max(index)));
                }
            }
            if let Block::ProviderOpaque { provider, data } = block
                && provider == GEMINI_PROVIDER_NAME
                && let OpaqueReplay::FunctionCall { call_id, .. } = parse_gemini_opaque(data)?
                && let Some(suffix) = call_id.strip_prefix(CALL_ID_PREFIX)
                && let Ok(index) = u64::from_str_radix(suffix, 16)
            {
                greatest = Some(greatest.map_or(index, |current: u64| current.max(index)));
            }
        }
    }
    Ok(greatest.map_or(count, |index| index.saturating_add(1)))
}

fn synthesized_call_id(index: u64) -> String {
    format!("{CALL_ID_PREFIX}{index:016x}")
}

#[derive(Debug, Clone)]
struct FunctionIdentity {
    name: String,
    args: serde_json::Value,
}

type CallNameIndex = HashMap<String, String>;
type OpaqueCallIndex = HashMap<String, FunctionIdentity>;
type ToolCallIndex = (CallNameIndex, OpaqueCallIndex);

pub(crate) fn gemini_request_json(
    request: &TurnRequest,
    effort: Option<&str>,
    web_builtins: bool,
) -> Result<serde_json::Value, ProviderError> {
    gemini_request_json_with_boundary(
        request,
        &request.tools,
        effort,
        web_builtins,
        request.messages.len(),
    )
    .map(|(payload, _, _)| payload)
}

fn gemini_request_json_with_boundary(
    request: &TurnRequest,
    tools: &[crate::ToolDefinition],
    effort: Option<&str>,
    web_builtins: bool,
    stable_history_end: usize,
) -> Result<
    (
        serde_json::Value,
        crate::PreparedHistoryBoundary,
        Option<crate::PreparedHistoryBoundary>,
    ),
    ProviderError,
> {
    let attachments = attachment_index(request)?;
    let (tool_names, opaque_calls) = tool_call_index(request)?;
    let mut contents = Vec::<serde_json::Value>::new();
    let mut pending_signed_text = VecDeque::<String>::new();
    let stable_history_end = stable_history_end.min(request.messages.len());
    let mut history_boundary =
        (stable_history_end == 0).then_some(crate::PreparedHistoryBoundary {
            items: 0,
            last_parts: 0,
        });
    let previous_history_end = request
        .cache_metadata
        .as_ref()
        .and_then(|metadata| metadata.previous_stable_history_end)
        .filter(|previous| *previous <= request.messages.len());
    let mut previous_history_boundary =
        (previous_history_end == Some(0)).then_some(crate::PreparedHistoryBoundary {
            items: 0,
            last_parts: 0,
        });

    for (message_index, message) in request.messages.iter().enumerate() {
        let role = match message.role {
            MessageRole::Assistant => "model",
            MessageRole::User | MessageRole::Tool => "user",
        };
        if role == "user" && !pending_signed_text.is_empty() {
            return Err(invalid_request(
                "Gemini signed text part is missing its normalized assistant text block",
            ));
        }
        let mut parts = Vec::new();
        for block in &message.blocks {
            match block {
                Block::Text { text } if message.role != MessageRole::Tool => {
                    if message.role == MessageRole::Assistant
                        && let Some(expected) = pending_signed_text.front()
                    {
                        if expected != text {
                            return Err(invalid_request(
                                "Gemini signed text part disagrees with normalized history",
                            ));
                        }
                        pending_signed_text.pop_front();
                    } else {
                        parts.push(serde_json::json!({"text": text}));
                    }
                }
                Block::Text { .. } => {
                    return Err(invalid_request(
                        "Gemini tool messages cannot contain plain text blocks",
                    ));
                }
                Block::Reasoning { .. } => {
                    return Err(invalid_request(
                        "normalized reasoning summaries cannot be replayed on the Gemini wire",
                    ));
                }
                Block::ToolCall {
                    call_id,
                    name,
                    args,
                } if message.role == MessageRole::Assistant => {
                    if let Some(native) = opaque_calls.get(call_id) {
                        if native.name != *name || native.args != *args {
                            return Err(invalid_request(
                                "Gemini signed function call disagrees with normalized history",
                            ));
                        }
                    } else {
                        parts.push(serde_json::json!({
                            "functionCall": {"name": name, "args": args}
                        }));
                    }
                }
                Block::ToolCall { .. } => {
                    return Err(invalid_request(
                        "Gemini function calls are only valid in assistant messages",
                    ));
                }
                Block::ToolResult {
                    call_id,
                    preview,
                    truncated,
                    images,
                } if matches!(message.role, MessageRole::User | MessageRole::Tool) => {
                    let name = tool_names.get(call_id).ok_or_else(|| {
                        invalid_request(format!(
                            "Gemini tool result references unknown call id `{call_id}`"
                        ))
                    })?;
                    parts.push(serde_json::json!({
                        "functionResponse": {
                            "name": name,
                            "response": {
                                "result": preview,
                                "truncated": truncated,
                            }
                        }
                    }));
                    for image in images {
                        if !crate::tool_image_media_type_supported(&image.media_type) {
                            return Err(invalid_request(format!(
                                "tool image {} has unsupported media type",
                                image.artifact
                            )));
                        }
                        let data = resolved_attachment(&attachments, &image.artifact)?;
                        parts.push(serde_json::json!({
                            "inlineData": {
                                "mimeType": image.media_type,
                                "data": data,
                            }
                        }));
                    }
                }
                Block::ToolResult { .. } => {
                    return Err(invalid_request(
                        "Gemini function responses are only valid in user/tool messages",
                    ));
                }
                Block::Attachment(AttachmentBlock::Image { artifact, mime, .. })
                    if message.role == MessageRole::User =>
                {
                    let data = resolved_attachment(&attachments, artifact)?;
                    parts.push(serde_json::json!({
                        "inlineData": {"mimeType": mime, "data": data}
                    }));
                }
                Block::Attachment(AttachmentBlock::Image { .. }) => {
                    return Err(invalid_request(
                        "Gemini image inputs are only valid in user messages",
                    ));
                }
                Block::Attachment(AttachmentBlock::PastedText { artifact, .. }) => {
                    return Err(invalid_request(format!(
                        "pasted-text attachment `{artifact}` was not resolved by the prompt compiler"
                    )));
                }
                Block::Attachment(AttachmentBlock::File { artifact, .. }) => {
                    return Err(invalid_request(format!(
                        "file attachment `{artifact}` was not resolved by the prompt compiler"
                    )));
                }
                Block::Attachment(AttachmentBlock::Pdf { artifact, .. }) => {
                    return Err(invalid_request(format!(
                        "PDF attachment `{artifact}` was not resolved by the prompt compiler"
                    )));
                }
                Block::Attachment(AttachmentBlock::Skill { name, .. }) => {
                    return Err(invalid_request(format!(
                        "skill attachment `{name}` was not resolved by the prompt compiler"
                    )));
                }
                Block::ProviderOpaque { provider, data }
                    if provider == GEMINI_PROVIDER_NAME
                        && message.role == MessageRole::Assistant =>
                {
                    match parse_gemini_opaque(data)? {
                        OpaqueReplay::FunctionCall { part, .. }
                        | OpaqueReplay::Thought { part } => parts.push(part),
                        OpaqueReplay::Text { text, part } => {
                            parts.push(part);
                            if !text.is_empty() {
                                pending_signed_text.push_back(text);
                            }
                        }
                    }
                }
                Block::ProviderOpaque { provider, .. } if provider == GEMINI_PROVIDER_NAME => {
                    return Err(invalid_request(
                        "Gemini provider-opaque blocks are only valid in assistant messages",
                    ));
                }
                Block::ProviderOpaque { provider, .. } => {
                    return Err(invalid_request(format!(
                        "provider-opaque block for `{provider}` cannot be sent to Gemini"
                    )));
                }
            }
        }
        append_content(&mut contents, role, parts)?;
        if message_index.saturating_add(1) == stable_history_end {
            history_boundary = Some(crate::PreparedHistoryBoundary {
                items: contents.len(),
                last_parts: contents
                    .last()
                    .and_then(|content| content.get("parts"))
                    .and_then(serde_json::Value::as_array)
                    .map_or(0, Vec::len),
            });
        }
        if previous_history_end == Some(message_index.saturating_add(1)) {
            previous_history_boundary = Some(crate::PreparedHistoryBoundary {
                items: contents.len(),
                last_parts: contents
                    .last()
                    .and_then(|content| content.get("parts"))
                    .and_then(serde_json::Value::as_array)
                    .map_or(0, Vec::len),
            });
        }
    }
    if !pending_signed_text.is_empty() {
        return Err(invalid_request(
            "Gemini signed text part is missing its normalized assistant text block",
        ));
    }

    let declarations = tools
        .iter()
        .map(|tool| {
            serde_json::json!({
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.input_schema,
            })
        })
        .collect::<Vec<_>>();
    // G3: `thinkingLevel` rides generationConfig for 3.x-named models when
    // the pinned static ladder declares the selected value; anything else
    // injects NOTHING (never `thinkingBudget`, never both fields).
    let thinking_level = effort.filter(|effort| {
        crate::effort::gemini_supported_efforts(&request.model)
            .iter()
            .any(|level| level == effort)
    });
    let generation_config = match thinking_level {
        Some(level) => serde_json::json!({
            "maxOutputTokens": request.max_tokens,
            "thinkingConfig": {"thinkingLevel": level},
        }),
        None => serde_json::json!({"maxOutputTokens": request.max_tokens}),
    };
    let history_boundary = history_boundary.unwrap_or_else(|| crate::PreparedHistoryBoundary {
        items: contents.len(),
        last_parts: contents
            .last()
            .and_then(|content| content.get("parts"))
            .and_then(serde_json::Value::as_array)
            .map_or(0, Vec::len),
    });
    let mut payload = serde_json::json!({
        "contents": contents,
        "generationConfig": generation_config,
    });
    let object = payload
        .as_object_mut()
        .ok_or_else(|| internal("Gemini request payload was not an object"))?;
    if let Some(system) = &request.system_prompt {
        object.insert(
            "system_instruction".into(),
            serde_json::json!({"parts": [{"text": system}]}),
        );
    }
    // W-B (LW5): the web built-ins join the SAME `tools` array as their own
    // entries, name-gated to 3.x models (the G3 pattern) — Gemini 3 models
    // COMBINE built-ins with function declarations; 2.5-era models cannot,
    // so they honestly declare neither regardless of the flag.
    let mut tool_entries = Vec::new();
    if !declarations.is_empty() {
        tool_entries.push(serde_json::json!({"functionDeclarations": declarations}));
    }
    if web_builtins && crate::effort::gemini_web_builtins_supported(&request.model) {
        tool_entries.push(serde_json::json!({"google_search": {}}));
        tool_entries.push(serde_json::json!({"url_context": {}}));
    }
    if !tool_entries.is_empty() {
        object.insert("tools".into(), serde_json::Value::Array(tool_entries));
    }
    Ok((payload, history_boundary, previous_history_boundary))
}

struct GeminiContentsPrefix<'a> {
    contents: &'a [serde_json::Value],
    boundary: crate::PreparedHistoryBoundary,
}

impl Serialize for GeminiContentsPrefix<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let end = self.boundary.items.min(self.contents.len());
        let mut sequence = serializer.serialize_seq(Some(end))?;
        for (index, content) in self.contents[..end].iter().enumerate() {
            if index.saturating_add(1) == end {
                sequence.serialize_element(&GeminiContentPrefix {
                    content,
                    parts_end: self.boundary.last_parts,
                })?;
            } else {
                sequence.serialize_element(content)?;
            }
        }
        sequence.end()
    }
}

struct GeminiContentPrefix<'a> {
    content: &'a serde_json::Value,
    parts_end: usize,
}

impl Serialize for GeminiContentPrefix<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let Some(object) = self.content.as_object() else {
            return self.content.serialize(serializer);
        };
        let mut map = serializer.serialize_map(Some(object.len()))?;
        for (key, value) in object {
            if key == "parts" {
                if let Some(parts) = value.as_array() {
                    map.serialize_entry(key, &parts[..self.parts_end.min(parts.len())])?;
                } else {
                    map.serialize_entry(key, value)?;
                }
            } else {
                map.serialize_entry(key, value)?;
            }
        }
        map.end()
    }
}

fn gemini_history_digest(
    full_payload: &serde_json::Value,
    boundary: crate::PreparedHistoryBoundary,
) -> Option<String> {
    let contents = payload_contents(full_payload)?;
    Some(crate::exact_optional_wire_digest(Some(
        &GeminiContentsPrefix { contents, boundary },
    )))
}

fn gemini_provider_view_blocks(
    contents: &[serde_json::Value],
    boundary: crate::PreparedHistoryBoundary,
) -> Option<Vec<Vec<u8>>> {
    let end = boundary.items.min(contents.len());
    contents[..end]
        .iter()
        .enumerate()
        .map(|(index, content)| {
            crate::serialize_json_fragment(&GeminiContentPrefix {
                content,
                parts_end: if index + 1 == end {
                    boundary.last_parts
                } else {
                    usize::MAX
                },
            })
        })
        .collect()
}

fn gemini_previous_provider_view_block_refs(
    contents: &[serde_json::Value],
    current_boundary: crate::PreparedHistoryBoundary,
    current_blocks: &[Vec<u8>],
    previous_boundary: crate::PreparedHistoryBoundary,
) -> Option<(
    Vec<haider_protocol::cache::ProviderViewBlockRefV1>,
    Option<usize>,
)> {
    let previous_end = previous_boundary.items.min(contents.len());
    let current_end = current_boundary.items.min(contents.len());
    let mut all_reused = previous_end <= current_blocks.len();
    let mut refs = Vec::with_capacity(previous_end);
    for (index, content) in contents[..previous_end].iter().enumerate() {
        let previous_parts_end = if index + 1 == previous_end {
            previous_boundary.last_parts
        } else {
            usize::MAX
        };
        let current_parts_end = if index + 1 == current_end {
            current_boundary.last_parts
        } else {
            usize::MAX
        };
        let parts_len = content
            .get("parts")
            .and_then(serde_json::Value::as_array)
            .map(Vec::len);
        let same_projection = index < current_blocks.len()
            && parts_len.is_none_or(|parts_len| {
                previous_parts_end.min(parts_len) == current_parts_end.min(parts_len)
            });
        if same_projection {
            refs.push(haider_protocol::cache::ProviderViewBlockRefV1::for_bytes(
                &current_blocks[index],
            ));
        } else {
            all_reused = false;
            refs.push(crate::exact_wire_block_ref(&GeminiContentPrefix {
                content,
                parts_end: previous_parts_end,
            })?);
        }
    }
    Some((refs, all_reused.then_some(previous_end)))
}

fn gemini_cacheable_contents(
    full_payload: &serde_json::Value,
    boundary: crate::PreparedHistoryBoundary,
) -> Option<&[serde_json::Value]> {
    let contents = payload_contents(full_payload)?;
    let end = boundary.items.min(contents.len());
    if end == 0 {
        return Some(&[]);
    }
    let final_parts = contents[end - 1].get("parts")?.as_array()?.len();
    if final_parts != boundary.last_parts {
        return None;
    }
    Some(&contents[..end])
}

fn gemini_content_block_refs(
    contents: &[serde_json::Value],
) -> Option<Vec<haider_protocol::cache::ProviderViewBlockRefV1>> {
    contents.iter().map(crate::exact_wire_block_ref).collect()
}

fn append_content(
    contents: &mut Vec<serde_json::Value>,
    role: &str,
    parts: Vec<serde_json::Value>,
) -> Result<(), ProviderError> {
    if parts.is_empty() {
        return Ok(());
    }
    if let Some(previous) = contents.last_mut()
        && previous.get("role").and_then(serde_json::Value::as_str) == Some(role)
    {
        let previous_parts = previous
            .get_mut("parts")
            .and_then(serde_json::Value::as_array_mut)
            .ok_or_else(|| internal("Gemini content parts were not an array"))?;
        previous_parts.extend(parts);
    } else {
        contents.push(serde_json::json!({"role": role, "parts": parts}));
    }
    Ok(())
}

fn attachment_index(request: &TurnRequest) -> Result<HashMap<&str, &str>, ProviderError> {
    let mut attachments = HashMap::new();
    for attachment in &request.attachments {
        if attachments
            .insert(
                attachment.artifact.as_str(),
                attachment.data_base64.as_str(),
            )
            .is_some()
        {
            return Err(invalid_request(format!(
                "attachment `{}` was resolved more than once",
                attachment.artifact
            )));
        }
    }
    Ok(attachments)
}

fn resolved_attachment<'a>(
    attachments: &'a HashMap<&str, &str>,
    artifact: &ArtifactRef,
) -> Result<&'a str, ProviderError> {
    attachments.get(artifact.as_str()).copied().ok_or_else(|| {
        invalid_request(format!(
            "image attachment `{artifact}` has no resolved provider bytes"
        ))
    })
}

fn tool_call_index(request: &TurnRequest) -> Result<ToolCallIndex, ProviderError> {
    let mut names = HashMap::<String, String>::new();
    let mut opaque = HashMap::<String, FunctionIdentity>::new();
    for message in &request.messages {
        for block in &message.blocks {
            match block {
                Block::ToolCall {
                    call_id,
                    name,
                    args,
                } => {
                    insert_call_name(&mut names, call_id, name)?;
                    if let Some(native) = opaque.get(call_id)
                        && (native.name != *name || native.args != *args)
                    {
                        return Err(invalid_request(
                            "Gemini signed function call disagrees with normalized history",
                        ));
                    }
                }
                Block::ProviderOpaque { provider, data } if provider == GEMINI_PROVIDER_NAME => {
                    if let OpaqueReplay::FunctionCall {
                        call_id,
                        name,
                        args,
                        ..
                    } = parse_gemini_opaque(data)?
                    {
                        insert_call_name(&mut names, &call_id, &name)?;
                        if let Some(existing) = opaque.insert(
                            call_id,
                            FunctionIdentity {
                                name: name.clone(),
                                args: args.clone(),
                            },
                        ) && (existing.name != name || existing.args != args)
                        {
                            return Err(invalid_request(
                                "Gemini history reuses a call id for different signed calls",
                            ));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    Ok((names, opaque))
}

fn insert_call_name(
    names: &mut HashMap<String, String>,
    call_id: &str,
    name: &str,
) -> Result<(), ProviderError> {
    if let Some(existing) = names.insert(call_id.to_owned(), name.to_owned())
        && existing != name
    {
        return Err(invalid_request(format!(
            "Gemini history reuses call id `{call_id}` for different functions"
        )));
    }
    Ok(())
}

enum OpaqueReplay {
    FunctionCall {
        call_id: String,
        name: String,
        args: serde_json::Value,
        part: serde_json::Value,
    },
    Text {
        text: String,
        part: serde_json::Value,
    },
    Thought {
        part: serde_json::Value,
    },
}

fn parse_gemini_opaque(data: &serde_json::Value) -> Result<OpaqueReplay, ProviderError> {
    let object = data.as_object().ok_or_else(|| {
        invalid_request("Gemini provider-opaque continuation must be a JSON object")
    })?;
    if object.get("kind").and_then(serde_json::Value::as_str) != Some(OPAQUE_KIND) {
        return Err(invalid_request(
            "Gemini provider-opaque continuation has an unsupported kind",
        ));
    }
    let part = object
        .get("part")
        .cloned()
        .ok_or_else(|| invalid_request("Gemini provider-opaque continuation has no native part"))?;
    let part_object = part.as_object().ok_or_else(|| {
        invalid_request("Gemini provider-opaque native part must be a JSON object")
    })?;
    if part_object
        .get("thoughtSignature")
        .and_then(serde_json::Value::as_str)
        .is_none_or(str::is_empty)
    {
        return Err(invalid_request(
            "Gemini provider-opaque native part has no thought signature",
        ));
    }
    if let Some(function) = part_object
        .get("functionCall")
        .and_then(serde_json::Value::as_object)
    {
        let call_id = object
            .get("call_id")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                invalid_request("Gemini signed function-call continuation has no call id")
            })?;
        let name = function
            .get("name")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| invalid_request("Gemini signed function call has no name"))?;
        let args = function
            .get("args")
            .cloned()
            .ok_or_else(|| invalid_request("Gemini signed function call has no args"))?;
        return Ok(OpaqueReplay::FunctionCall {
            call_id: call_id.to_owned(),
            name: name.to_owned(),
            args,
            part,
        });
    }
    if let Some(text) = part_object.get("text").and_then(serde_json::Value::as_str) {
        if part_object
            .get("thought")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            Ok(OpaqueReplay::Thought { part })
        } else {
            Ok(OpaqueReplay::Text {
                text: text.to_owned(),
                part,
            })
        }
    } else {
        Ok(OpaqueReplay::Thought { part })
    }
}

pub(crate) trait GeminiSseChunkSource {
    async fn next_chunk(
        &mut self,
    ) -> Result<Option<impl AsRef<[u8]> + Send + 'static>, ProviderError>;
}

impl GeminiSseChunkSource for reqwest::Response {
    async fn next_chunk(
        &mut self,
    ) -> Result<Option<impl AsRef<[u8]> + Send + 'static>, ProviderError> {
        self.chunk().await.map_err(transport_error)
    }
}

pub(crate) async fn stream_sse_source<S: GeminiSseChunkSource>(
    mut source: S,
    account: Option<CredentialAlias>,
    next_call_index: u64,
    sender: mpsc::Sender<ProviderStreamItem>,
    chunk_idle_timeout: Duration,
    semantic_progress_timeout: Duration,
) {
    let mut decoder = GeminiDecoder::new(account, next_call_index);
    let mut progress = crate::ProviderProgressClock::new(
        chunk_idle_timeout,
        semantic_progress_timeout,
        crate::RouteGating::Enabled,
    );
    loop {
        let chunk = match progress.wait_for_next(source.next_chunk(), &sender).await {
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
                        "Gemini",
                        semantic_progress_timeout,
                    )))
                    .await;
                return;
            }
        };
        progress.observe_raw_chunk();
        let items = decoder.push(chunk.as_ref());
        if crate::has_semantic_progress(&items) {
            progress.observe_semantic_progress();
        }
        if !send_items(&sender, items).await || decoder.terminal {
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

#[derive(Debug)]
struct SseFrame {
    data: String,
}

#[derive(Debug, Default)]
struct SseFramer {
    utf8: Utf8Assembler,
    line_buffer: String,
    data_lines: Vec<String>,
}

impl SseFramer {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseFrame>, ProviderError> {
        let decoded = self.utf8.push(bytes)?;
        let mut frames = Vec::new();
        for text in decoded {
            self.line_buffer.push_str(&text);
            while let Some(newline) = self.line_buffer.find('\n') {
                let mut line = self.line_buffer.drain(..=newline).collect::<String>();
                line.pop();
                if line.ends_with('\r') {
                    line.pop();
                }
                if let Some(frame) = self.accept_line(&line) {
                    frames.push(frame);
                }
            }
        }
        Ok(frames)
    }

    fn finish(&mut self) -> Result<Vec<SseFrame>, ProviderError> {
        if self.utf8.has_pending() {
            return Err(malformed("Gemini SSE stream ended inside a UTF-8 scalar"));
        }
        let mut frames = Vec::new();
        if !self.line_buffer.is_empty() {
            let line = std::mem::take(&mut self.line_buffer);
            if let Some(frame) = self.accept_line(line.trim_end_matches('\r')) {
                frames.push(frame);
            }
        }
        if let Some(frame) = self.dispatch_frame() {
            frames.push(frame);
        }
        Ok(frames)
    }

    fn accept_line(&mut self, line: &str) -> Option<SseFrame> {
        if line.is_empty() {
            return self.dispatch_frame();
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = line.split_once(':').map_or((line, ""), |(field, value)| {
            (field, value.strip_prefix(' ').unwrap_or(value))
        });
        if field == "data" {
            self.data_lines.push(value.to_owned());
        }
        None
    }

    fn dispatch_frame(&mut self) -> Option<SseFrame> {
        if self.data_lines.is_empty() {
            return None;
        }
        let frame = SseFrame {
            data: self.data_lines.join("\n"),
        };
        self.data_lines.clear();
        Some(frame)
    }
}

#[derive(Debug)]
pub(crate) struct GeminiDecoder {
    framer: SseFramer,
    account: Option<CredentialAlias>,
    next_call_index: u64,
    saw_tool: bool,
    saw_refusal: bool,
    terminal: bool,
    /// W-B: grounding facts repeat across frames — queries and source URLs
    /// already surfaced must not mint duplicate rows or sources.
    seen_search_queries: HashSet<String>,
    seen_source_urls: HashSet<String>,
    search_rows: u64,
}

impl GeminiDecoder {
    pub(crate) fn new(account: Option<CredentialAlias>, next_call_index: u64) -> Self {
        Self {
            framer: SseFramer::default(),
            account,
            next_call_index,
            saw_tool: false,
            saw_refusal: false,
            terminal: false,
            seen_search_queries: HashSet::new(),
            seen_source_urls: HashSet::new(),
            search_rows: 0,
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<ProviderStreamItem> {
        if self.terminal {
            return Vec::new();
        }
        let frames = match self.framer.push(bytes) {
            Ok(frames) => frames,
            Err(error) => return self.fail(error),
        };
        self.accept_frames(frames)
    }

    pub(crate) fn finish(&mut self) -> Vec<ProviderStreamItem> {
        if self.terminal {
            return Vec::new();
        }
        let frames = match self.framer.finish() {
            Ok(frames) => frames,
            Err(error) => return self.fail(error),
        };
        let mut items = self.accept_frames(frames);
        if !self.terminal {
            items.push(Err(stream_interrupted(
                "Gemini SSE stream ended before a terminal finish reason",
            )));
            self.terminal = true;
        }
        items
    }

    fn accept_frames(&mut self, frames: Vec<SseFrame>) -> Vec<ProviderStreamItem> {
        let mut output = Vec::new();
        for frame in frames {
            let items = match self.dispatch(frame) {
                Ok(items) => items.into_iter().map(Ok).collect::<Vec<_>>(),
                Err(error) => vec![Err(error)],
            };
            let terminal = items
                .iter()
                .any(|item| matches!(item, Err(_) | Ok(StreamEvent::Finish { .. })));
            output.extend(items);
            if terminal {
                self.terminal = true;
                break;
            }
        }
        output
    }

    fn dispatch(&mut self, frame: SseFrame) -> Result<Vec<StreamEvent>, ProviderError> {
        let value: serde_json::Value = serde_json::from_str(&frame.data)
            .map_err(|error| malformed(format!("Gemini SSE data is not valid JSON: {error}")))?;
        if value
            .get("promptFeedback")
            .and_then(|feedback| feedback.get("blockReason"))
            .and_then(serde_json::Value::as_str)
            .filter(|reason| *reason != "BLOCK_REASON_UNSPECIFIED")
            .is_some()
        {
            self.saw_refusal = true;
            return Ok(vec![
                safety_refusal_delta(),
                StreamEvent::Finish {
                    reason: FinishReason::Refusal,
                },
            ]);
        }

        let mut events = Vec::new();
        if let Some(candidate) = value
            .get("candidates")
            .and_then(serde_json::Value::as_array)
            .and_then(|candidates| candidates.first())
            && let Some(parts) = candidate
                .get("content")
                .and_then(|content| content.get("parts"))
                .and_then(serde_json::Value::as_array)
        {
            for part in parts {
                events.extend(self.part_events(part)?);
            }
        }
        // W-B (LW5): groundingMetadata / url_context_metadata decode into
        // display rows and sources — tolerant to absent fields and to either
        // field casing (the REST casing is camelCase; snake_case is accepted
        // because the research doc could only infer the url_context spelling).
        if let Some(candidate) = value
            .get("candidates")
            .and_then(serde_json::Value::as_array)
            .and_then(|candidates| candidates.first())
        {
            let grounding = self.grounding_events(candidate);
            events.extend(grounding);
        }
        if let Some(usage) = value.get("usageMetadata") {
            events.push(StreamEvent::UsageUpdate(gemini_usage(
                usage,
                self.account.clone(),
            )?));
        }
        if let Some(reason) = value
            .get("candidates")
            .and_then(serde_json::Value::as_array)
            .and_then(|candidates| candidates.first())
            .and_then(|candidate| candidate.get("finishReason"))
            .and_then(serde_json::Value::as_str)
        {
            let finish = match reason {
                "STOP" => {
                    if self.saw_tool {
                        FinishReason::ToolUse
                    } else {
                        FinishReason::EndTurn
                    }
                }
                "MAX_TOKENS" => FinishReason::MaxTokens,
                "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII"
                | "IMAGE_SAFETY" => {
                    if !self.saw_refusal {
                        events.push(safety_refusal_delta());
                        self.saw_refusal = true;
                    }
                    FinishReason::Refusal
                }
                "OTHER"
                | "LANGUAGE"
                | "MALFORMED_FUNCTION_CALL"
                | "UNEXPECTED_TOOL_CALL"
                | "NO_IMAGE"
                | "IMAGE_PROHIBITED_CONTENT" => FinishReason::Error,
                _ => {
                    return Err(malformed(format!(
                        "Gemini response used unknown finish reason `{reason}`"
                    )));
                }
            };
            events.push(StreamEvent::Finish { reason: finish });
        }
        Ok(events)
    }

    fn part_events(&mut self, part: &serde_json::Value) -> Result<Vec<StreamEvent>, ProviderError> {
        let object = part
            .as_object()
            .ok_or_else(|| malformed("Gemini response part is not a JSON object"))?;
        let signature = object
            .get("thoughtSignature")
            .and_then(serde_json::Value::as_str)
            .filter(|signature| !signature.is_empty());
        if let Some(function) = object
            .get("functionCall")
            .and_then(serde_json::Value::as_object)
        {
            let name = function
                .get("name")
                .and_then(serde_json::Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| malformed("Gemini function call has no name"))?;
            let args = function
                .get("args")
                .cloned()
                .ok_or_else(|| malformed("Gemini function call has no args"))?;
            if !args.is_object() {
                return Err(malformed("Gemini function-call args are not an object"));
            }
            let call_id = synthesized_call_id(self.next_call_index);
            self.next_call_index = self.next_call_index.saturating_add(1);
            self.saw_tool = true;
            let mut events = Vec::new();
            if signature.is_some() {
                events.push(StreamEvent::ProviderOpaque {
                    provider: GEMINI_PROVIDER_NAME.into(),
                    data: serde_json::json!({
                        "kind": OPAQUE_KIND,
                        "call_id": call_id,
                        "part": part,
                    }),
                });
            }
            events.push(StreamEvent::ToolCallStart {
                call_id: call_id.clone(),
                name: name.to_owned(),
            });
            events.push(StreamEvent::ToolCallArgsDelta {
                call_id: call_id.clone(),
                args_fragment: serde_json::to_string(&args).map_err(|error| {
                    malformed(format!(
                        "Gemini function-call args could not be encoded: {error}"
                    ))
                })?,
            });
            events.push(StreamEvent::ToolCallEnd { call_id });
            return Ok(events);
        }
        if let Some(text) = object.get("text").and_then(serde_json::Value::as_str) {
            let mut events = Vec::new();
            if signature.is_some() {
                events.push(StreamEvent::ProviderOpaque {
                    provider: GEMINI_PROVIDER_NAME.into(),
                    data: serde_json::json!({"kind": OPAQUE_KIND, "part": part}),
                });
            }
            if !text.is_empty() {
                if object
                    .get("thought")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
                {
                    events.push(StreamEvent::ReasoningDelta { text: text.into() });
                } else {
                    events.push(StreamEvent::TextDelta { text: text.into() });
                }
            }
            return Ok(events);
        }
        if signature.is_some() {
            return Ok(vec![StreamEvent::ProviderOpaque {
                provider: GEMINI_PROVIDER_NAME.into(),
                data: serde_json::json!({"kind": OPAQUE_KIND, "part": part}),
            }]);
        }
        Ok(Vec::new())
    }

    /// Decodes one candidate's grounding facts (W-B): executed search
    /// queries become closed display rows, groundingChunks web sources and
    /// successfully retrieved url_context URLs become display sources. Every
    /// field is optional — absence never fails the stream — and repeats
    /// across frames dedup.
    fn grounding_events(&mut self, candidate: &serde_json::Value) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        let mut sources = Vec::new();
        if let Some(grounding) = candidate
            .get("groundingMetadata")
            .or_else(|| candidate.get("grounding_metadata"))
        {
            if let Some(queries) = grounding
                .get("webSearchQueries")
                .or_else(|| grounding.get("web_search_queries"))
                .and_then(serde_json::Value::as_array)
            {
                for query in queries {
                    let Some(query) = query.as_str() else {
                        continue;
                    };
                    if !self.seen_search_queries.insert(query.to_owned()) {
                        continue;
                    }
                    self.search_rows = self.search_rows.saturating_add(1);
                    let call_id = format!("gemini-search-{}", self.search_rows);
                    events.push(StreamEvent::ServerToolUse {
                        call_id: call_id.clone(),
                        name: "web_search".into(),
                        args: serde_json::json!({"query": query}),
                    });
                    events.push(StreamEvent::ServerToolResult {
                        call_id,
                        preview: "grounded".into(),
                        is_error: false,
                    });
                }
            }
            if let Some(chunks) = grounding
                .get("groundingChunks")
                .or_else(|| grounding.get("grounding_chunks"))
                .and_then(serde_json::Value::as_array)
            {
                for chunk in chunks {
                    let Some(web) = chunk.get("web") else {
                        continue;
                    };
                    let Some(uri) = web.get("uri").and_then(serde_json::Value::as_str) else {
                        continue;
                    };
                    if !self.seen_source_urls.insert(uri.to_owned()) {
                        continue;
                    }
                    sources.push(WebSource {
                        url: uri.to_owned(),
                        title: web
                            .get("title")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned),
                    });
                }
            }
        }
        if let Some(entries) = candidate
            .get("urlContextMetadata")
            .or_else(|| candidate.get("url_context_metadata"))
            .and_then(|metadata| {
                metadata
                    .get("urlMetadata")
                    .or_else(|| metadata.get("url_metadata"))
            })
            .and_then(serde_json::Value::as_array)
        {
            for entry in entries {
                let url = entry
                    .get("retrievedUrl")
                    .or_else(|| entry.get("retrieved_url"))
                    .and_then(serde_json::Value::as_str);
                let Some(url) = url else { continue };
                let status = entry
                    .get("urlRetrievalStatus")
                    .or_else(|| entry.get("url_retrieval_status"))
                    .and_then(serde_json::Value::as_str);
                if status.is_some_and(|status| !status.ends_with("SUCCESS")) {
                    continue;
                }
                if !self.seen_source_urls.insert(url.to_owned()) {
                    continue;
                }
                sources.push(WebSource {
                    url: url.to_owned(),
                    title: None,
                });
            }
        }
        if !sources.is_empty() {
            events.push(StreamEvent::WebSources { sources });
        }
        events
    }

    fn fail(&mut self, error: ProviderError) -> Vec<ProviderStreamItem> {
        self.terminal = true;
        vec![Err(error)]
    }
}

fn safety_refusal_delta() -> StreamEvent {
    StreamEvent::RefusalDelta {
        text: "Gemini blocked the response for safety reasons.".into(),
    }
}

fn gemini_usage(
    value: &serde_json::Value,
    account: Option<CredentialAlias>,
) -> Result<Usage, ProviderError> {
    let read = |field: &str| -> Result<u64, ProviderError> {
        value.get(field).map_or(Ok(0), |value| {
            value
                .as_u64()
                .ok_or_else(|| malformed(format!("Gemini usage field `{field}` is not an integer")))
        })
    };
    let optional = |field: &str| -> Result<Option<u64>, ProviderError> {
        value.get(field).map_or(Ok(None), |value| {
            value
                .as_u64()
                .map(Some)
                .ok_or_else(|| malformed(format!("Gemini usage field `{field}` is not an integer")))
        })
    };
    let logical_input = read("promptTokenCount")?;
    let output = read("candidatesTokenCount")?;
    let reasoning = optional("thoughtsTokenCount")?;
    let cached = optional("cachedContentTokenCount")?;
    let normalized = match cached.filter(|cached| *cached <= logical_input) {
        Some(cached) => NormalizedUsage {
            logical_input,
            uncached_input: logical_input - cached,
            cache_read_input: cached,
            billed_output: output,
            reasoning_detail: reasoning.unwrap_or(0),
            reasoning_accounting: reasoning.map_or(ReasoningAccounting::Unavailable, |_| {
                ReasoningAccounting::AdditionalToOutput
            }),
            cache_status: CacheStatAvailability::Present,
            cache_telemetry_input: logical_input,
            ..NormalizedUsage::default()
        },
        None => NormalizedUsage {
            logical_input,
            uncached_input: logical_input,
            billed_output: output,
            reasoning_detail: reasoning.unwrap_or(0),
            reasoning_accounting: reasoning.map_or(ReasoningAccounting::Unavailable, |_| {
                ReasoningAccounting::AdditionalToOutput
            }),
            ..NormalizedUsage::default()
        },
    };
    Ok(Usage {
        input: logical_input,
        output,
        reasoning: reasoning.unwrap_or(0),
        cached: normalized.cache_read_input,
        source: UsageSource::ProviderReported,
        account,
        accounts: Vec::new(),
        normalized: Some(normalized),
        scope: None,
        cache_cost: None,
        request: None,
    })
}

/// Replay captured data-only SSE through the same 7-byte incremental path.
/// Replays a captured stream with the SAME call-index continuation the
/// production path derives from the request history — the seam that makes
/// sparse/replayed call indices testable (a dense one-call history is
/// DEGENERATE: `count == greatest + 1` there, so only a sparse history can
/// distinguish continuation from restart).
pub fn replay_gemini_sse_for_request(
    request: &TurnRequest,
    bytes: &[u8],
) -> Result<Vec<ProviderStreamItem>, ProviderError> {
    let next_call_index = next_synthesized_call_index(request)?;
    let mut decoder = GeminiDecoder::new(None, next_call_index);
    let mut items = Vec::new();
    for chunk in bytes.chunks(7) {
        items.extend(decoder.push(chunk));
        if decoder.terminal {
            return Ok(items);
        }
    }
    items.extend(decoder.finish());
    Ok(items)
}

pub fn replay_gemini_sse(bytes: &[u8]) -> Vec<ProviderStreamItem> {
    let mut decoder = GeminiDecoder::new(None, 0);
    let mut items = Vec::new();
    for chunk in bytes.chunks(7) {
        items.extend(decoder.push(chunk));
        if decoder.terminal {
            return items;
        }
    }
    items.extend(decoder.finish());
    items
}

/// Replay a captured non-success response through the live typed classifier.
#[must_use]
pub fn replay_gemini_http_error(
    status: u16,
    retry_after: Option<&str>,
    body: &[u8],
) -> ProviderError {
    let parsed = serde_json::from_slice::<serde_json::Value>(body).ok();
    let api_error = parsed.as_ref().and_then(|value| value.get("error"));
    let wire_status = api_error
        .and_then(|error| error.get("status"))
        .and_then(serde_json::Value::as_str);
    let detail = api_error
        .and_then(|error| error.get("message"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|message| !message.is_empty());
    let context_exceeded = status == 400
        && wire_status == Some("INVALID_ARGUMENT")
        && detail.is_some_and(is_context_prose);
    let overloaded_prose = detail.is_some_and(|message| {
        let lower = message.to_ascii_lowercase();
        lower.contains("overload") || lower.contains("unavailable")
    });
    let billing_exhausted =
        wire_status == Some("RESOURCE_EXHAUSTED") && gemini_billing_exhausted(api_error, detail);
    let kind = match status {
        _ if billing_exhausted => ProviderErrorKind::QuotaExhausted,
        401 => ProviderErrorKind::Authentication,
        403 => ProviderErrorKind::PermissionDenied,
        429 => ProviderErrorKind::RateLimited,
        503 => ProviderErrorKind::Overloaded,
        _ if context_exceeded => ProviderErrorKind::ContextExceeded,
        _ if overloaded_prose => ProviderErrorKind::Overloaded,
        500..=599 => ProviderErrorKind::Transport,
        _ => match wire_status {
            Some("UNAUTHENTICATED") => ProviderErrorKind::Authentication,
            Some("PERMISSION_DENIED") => ProviderErrorKind::PermissionDenied,
            Some("RESOURCE_EXHAUSTED") => ProviderErrorKind::RateLimited,
            Some("UNAVAILABLE") => ProviderErrorKind::Overloaded,
            _ => ProviderErrorKind::InvalidRequest,
        },
    };
    let retry_after_ms = if matches!(kind, ProviderErrorKind::RateLimited) {
        parse_retry_info(api_error).or_else(|| crate::parse_retry_after_ms(retry_after))
    } else {
        None
    };
    let message = if kind == ProviderErrorKind::QuotaExhausted {
        "provider quota/credit exhausted — retrying will not help; check billing or switch account"
            .to_owned()
    } else {
        format!("Gemini HTTP {status} returned {}", provider_kind_name(kind))
    };
    ProviderError::new(kind, message)
        .with_retry_after_ms(retry_after_ms)
        .with_http_metadata(status, None)
}

fn gemini_billing_exhausted(api_error: Option<&serde_json::Value>, detail: Option<&str>) -> bool {
    let detail_is_billing = detail.is_some_and(|message| {
        let lower = message.to_ascii_lowercase();
        lower.contains("billing") || lower.contains("credit") || lower.contains("payment")
    });
    detail_is_billing
        || api_error
            .and_then(|error| error.get("details"))
            .and_then(serde_json::Value::as_array)
            .is_some_and(|details| {
                details.iter().any(|detail| {
                    detail
                        .get("reason")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|reason| {
                            let upper = reason.to_ascii_uppercase();
                            upper.contains("BILLING")
                                || upper.contains("CREDIT")
                                || upper.contains("PAYMENT")
                        })
                })
            })
}

fn is_context_prose(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    [
        "token count",
        "input token",
        "too many tokens",
        "context window",
        "context length",
        "maximum number of tokens",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn parse_retry_info(error: Option<&serde_json::Value>) -> Option<u64> {
    let details = error?.get("details")?.as_array()?;
    details.iter().find_map(|detail| {
        let kind = detail.get("@type")?.as_str()?;
        kind.ends_with("google.rpc.RetryInfo")
            .then(|| detail.get("retryDelay")?.as_str())
            .flatten()
            .and_then(parse_protobuf_duration_ms)
    })
}

pub(crate) fn parse_protobuf_duration_ms(value: &str) -> Option<u64> {
    let seconds = value.strip_suffix('s')?;
    let (whole, fraction) = seconds.split_once('.').unwrap_or((seconds, ""));
    let whole = whole.parse::<u64>().ok()?;
    let fraction_digits =
        fraction
            .bytes()
            .take(3)
            .try_fold((0_u64, 0_usize), |(number, count), byte| {
                if byte.is_ascii_digit() {
                    Some((number * 10 + u64::from(byte - b'0'), count + 1))
                } else {
                    None
                }
            })?;
    if fraction.bytes().any(|byte| !byte.is_ascii_digit()) {
        return None;
    }
    let millis = match fraction_digits.1 {
        0 => 0,
        1 => fraction_digits.0 * 100,
        2 => fraction_digits.0 * 10,
        _ => fraction_digits.0,
    };
    whole.checked_mul(1_000)?.checked_add(millis)
}

async fn read_error_body_bounded(response: reqwest::Response) -> Result<Vec<u8>, ProviderError> {
    crate::read_http_error_body_bounded(response, "Gemini").await
}

fn classify_http_body_read_error(
    status: u16,
    retry_after: Option<&str>,
    mut error: ProviderError,
) -> ProviderError {
    // Receiving an HTTP status completes transport classification. A later
    // diagnostic-body reset cannot turn the provider response into route loss.
    let classified = replay_gemini_http_error(status, retry_after, &[]);
    error.kind = classified.kind;
    error.retryable = classified.retryable;
    error.retry_after_ms = classified.retry_after_ms;
    error.presentation = classified.presentation;
    error
}

fn provider_kind_name(kind: ProviderErrorKind) -> &'static str {
    match kind {
        ProviderErrorKind::Authentication => "authentication failure",
        ProviderErrorKind::PermissionDenied => "permission denial",
        ProviderErrorKind::RateLimited => "rate limit",
        ProviderErrorKind::Overloaded => "overload",
        ProviderErrorKind::ContextExceeded => "context overflow",
        ProviderErrorKind::InvalidRequest => "invalid request",
        ProviderErrorKind::NetworkUnavailable => "local network unavailability",
        ProviderErrorKind::Transport => "transport failure",
        ProviderErrorKind::MalformedFrame => "malformed frame",
        ProviderErrorKind::InvalidUtf8 => "invalid UTF-8",
        ProviderErrorKind::Internal => "internal failure",
        ProviderErrorKind::QuotaExhausted => "quota/credit exhaustion",
        ProviderErrorKind::StreamInterrupted => "stream interruption",
        ProviderErrorKind::ConnectionConfiguration => "connection configuration failure",
    }
}

fn transport_error(error: reqwest::Error) -> ProviderError {
    crate::reqwest_transport_error_with_route_gating("Gemini", error, crate::RouteGating::Enabled)
}

fn response_open_timeout_error(timeout: Duration) -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Transport,
        format!(
            "Gemini response did not open within {} seconds",
            timeout.as_secs()
        ),
    )
}

fn stream_idle_error(timeout: Duration) -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Transport,
        format!(
            "Gemini SSE stream received no data for {} seconds",
            timeout.as_secs()
        ),
    )
}

fn invalid_request(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ProviderErrorKind::InvalidRequest, message)
}

fn malformed(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ProviderErrorKind::MalformedFrame, message)
}

fn stream_interrupted(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ProviderErrorKind::StreamInterrupted, message)
}

fn internal(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ProviderErrorKind::Internal, message)
}
