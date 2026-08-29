//! OpenAI Responses API and OpenAI-compatible Chat Completions adapters.
//!
//! The native adapter uses Responses because its typed output-item stream maps
//! directly to Haider's text, reasoning-summary, tool-call, usage, and finish
//! events. The compatible adapter reuses the same transport policy but speaks
//! Chat Completions, the common wire implemented by vLLM, Ollama, LM Studio,
//! LiteLLM, TGI, Hugging Face endpoints, and generic gateways.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use haider_accounts::SecretHandle;
use haider_protocol::computer::{ComputerAction, ScreenPoint, ScrollDirection};
use haider_protocol::ids::CredentialAlias;
use haider_protocol::provider::{
    Block, CacheStatAvailability, CapabilityDoc, FeatureResolve, FinishReason, NormalizedUsage,
    ReasoningAccounting, StreamEvent, Usage, UsageSource, WebSource,
};
use haider_protocol::tool::AttachmentBlock;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderValue, RETRY_AFTER};
use serde::Deserialize;
use tokio::sync::{OnceCell, mpsc};

use crate::origin::{FixedDnsResolver, FixedOriginGuard, SystemFixedDnsResolver};
use crate::wire::provider_kind_name;
use crate::{
    MessageRole, Provider, ProviderError, ProviderErrorKind, ProviderStream, ProviderStreamItem,
    TurnRequest, Utf8Assembler,
};

pub const OPENAI_PROVIDER_NAME: &str = "openai";
pub const OPENAI_OAUTH_PROVIDER_NAME: &str = "openai-oauth";
pub const OPENAI_COMPATIBLE_PROVIDER_NAME: &str = "openai-compatible";
pub const KIMI_OAUTH_PROVIDER_NAME: &str = "kimi-oauth";
pub const KIMI_OAUTH_BASE_URL: &str = "https://api.kimi.com/coding/v1";
pub const DEEPSEEK_PROVIDER_NAME: &str = "deepseek";
pub const DEEPSEEK_BASE_URL: &str = "https://api.deepseek.com";
/// Documented compatibility aliases used only until authenticated discovery
/// returns the live inventory. V4 slugs deliberately come from `/models`
/// rather than this fallback.
pub const DEEPSEEK_SEED_MODELS: [&str; 2] = ["deepseek-chat", "deepseek-reasoner"];
pub const HAIDER_CODE_PROVIDER_NAME: &str = "haider-code";
pub const HAIDER_CODE_BASE_URL: &str = "https://haidercode.ai/v1";
pub const HAIDER_CODE_ACCOUNT_URL: &str = "https://haidercode.ai/v1/account";
pub const HAIDER_CODE_SEED_MODELS: [&str; 2] = ["Go", "Go Max"];
pub const XAI_PROVIDER_NAME: &str = "xai";
pub const XAI_BASE_URL: &str = "https://api.x.ai/v1";
const XAI_CONVERSATION_ID_HEADER: &str = "x-grok-conv-id";
pub const XAI_SEED_MODELS: [&str; 4] = ["grok-4.6", "grok-4.5", "grok-4.3", "grok-build-0.1"];
pub const XAI_SEED_MODEL_CONTEXT_WINDOWS: [(&str, u64); 4] = [
    ("grok-4.6", 500_000),
    ("grok-4.5", 500_000),
    ("grok-4.3", 1_000_000),
    ("grok-build-0.1", 256_000),
];
pub const GROK_OAUTH_PROVIDER_NAME: &str = "grok-oauth";
pub const GROK_OAUTH_BASE_URL: &str = "https://cli-chat-proxy.grok.com/v1";
/// Version admitted by the Grok subscription proxy. The proxy hard-gates
/// this value, so it may need bumping when xAI advances the grok-shell client.
pub const GROK_SHELL_CLIENT_VERSION: &str = "0.2.101";

/// The version actually sent — `HAIDER_GROK_CLIENT_VERSION` overrides the
/// pinned const, so a proxy-side rotation is a config change for the user
/// ('grok just works'), never a wait for the next harness release. Resolved
/// once per process.
pub fn grok_client_version() -> &'static str {
    static RESOLVED: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    RESOLVED
        .get_or_init(|| {
            std::env::var("HAIDER_GROK_CLIENT_VERSION")
                .ok()
                .map(|value| value.trim().to_owned())
                .filter(|value| {
                    !value.is_empty()
                        && value.len() <= 32
                        && value
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'.')
                })
                .unwrap_or_else(|| GROK_SHELL_CLIENT_VERSION.to_owned())
        })
        .as_str()
}
pub const GROK_SHELL_CLIENT_IDENTIFIER: &str = "grok-shell";
pub const GROK_SHELL_CLIENT_MODE: &str = "interactive";
pub const GROK_XAI_TOKEN_AUTH: &str = "xai-grok-cli";
pub const OPENAI_RESPONSES_API_URL: &str = "https://api.openai.com/v1/responses";
pub const OPENAI_SUBSCRIPTION_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
pub const OPENAI_SUBSCRIPTION_RESPONSES_URL: &str =
    "https://chatgpt.com/backend-api/codex/responses";
pub const OPENAI_CODEX_RESPONSES_LITE_HEADER: &str = "x-openai-internal-codex-responses-lite";
pub const OPENAI_CODEX_RESPONSES_LITE_VALUE: &str = "true";
const OPENAI_LITE_CACHE_BREAKPOINT_ENV: &str = "HAIDER_OPENAI_LITE_CACHE_BREAKPOINT";
/// W-B (decision 3): the UNOFFICIAL codex search endpoint the client
/// `web_search` tool executes against on lite pairs — same origin and
/// Bearer as subscription turns. Source-verified against codex main
/// 2026-08; a 404/410 degrades the capability for the session.
pub const OPENAI_ALPHA_SEARCH_URL: &str = "https://chatgpt.com/backend-api/codex/alpha/search";
const OPENAI_SUBSCRIPTION_HOST: &str = "chatgpt.com";
const KIMI_OAUTH_HOST: &str = "api.kimi.com";
const DEEPSEEK_HOST: &str = "api.deepseek.com";
const HAIDER_CODE_HOST: &str = "haidercode.ai";
const XAI_HOST: &str = "api.x.ai";
const GROK_OAUTH_HOST: &str = "cli-chat-proxy.grok.com";

// The preview tool requires an explicit display coordinate space before the
// first screenshot exists. Use one bounded 16:9 CU-1 bootstrap viewport, then
// advertise the exact dimensions of the newest admitted computer screenshot
// on every follow-up request. GA `computer` deliberately has no display
// fields: its coordinate space is established by the returned
// `computer_screenshot` with `detail: original`.
const OPENAI_COMPUTER_BOOTSTRAP_WIDTH: u32 = haider_protocol::tool::TOOL_RESULT_IMAGE_MAX_DIMENSION;
const OPENAI_COMPUTER_BOOTSTRAP_HEIGHT: u32 = 1_152;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenAiComputerToolKind {
    Generic,
    Preview,
    Ga,
}

const STREAM_CAPACITY: usize = 32;
const MODELS_BODY_LIMIT: usize = 1024 * 1024;
#[cfg(test)]
const ERROR_BODY_LIMIT: usize = crate::HTTP_ERROR_BODY_LIMIT;
/// Default OpenAI-family transport budgets. Connect and response-open are
/// deliberately separate: reasoning/MoE gateways may accept a TCP connection
/// promptly while taking substantially longer to produce response headers.
pub const OPENAI_DEFAULT_TRANSPORT_CONFIG: OpenAiTransportConfig = OpenAiTransportConfig {
    retry_policy: OpenAiRetryPolicy::Never,
    connect_timeout: Duration::from_secs(10),
    response_open_timeout: Duration::from_secs(60),
    chunk_idle_timeout: Duration::from_secs(90),
};

static OPENAI_CLIENT_BUILD_COUNT: AtomicUsize = AtomicUsize::new(0);

struct OpenAiFixedTransport {
    client: reqwest::Client,
    guard: Arc<FixedOriginGuard>,
}

struct OpenAiCompatibleTransport {
    client: reqwest::Client,
    guard: Option<Arc<CompatibleOriginGuard>>,
}

/// Guarded transport coordinates shared by OpenAI-compatible inference and
/// model discovery. Keeping construction here prevents `/v1/models` from
/// growing a weaker DNS, proxy, redirect, or URL-normalization path than
/// `/v1/chat/completions`.
pub(crate) struct OpenAiCompatibleCatalogTransport {
    pub(crate) client: reqwest::Client,
    pub(crate) models_url: String,
}

fn build_openai_client(
    origin_guard: Option<Arc<CompatibleOriginGuard>>,
    fixed_origin_guard: Option<Arc<FixedOriginGuard>>,
    transport: OpenAiTransportConfig,
) -> Result<reqwest::Client, ProviderError> {
    OPENAI_CLIENT_BUILD_COUNT.fetch_add(1, Ordering::Relaxed);
    let mut client = crate::provider_http_client_builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(match transport.retry_policy {
            OpenAiRetryPolicy::Never => reqwest::retry::never(),
        })
        .pool_idle_timeout(crate::PROVIDER_POOL_IDLE_TIMEOUT)
        .http2_adaptive_window(true)
        .connect_timeout(transport.connect_timeout);
    if let Some(guard) = origin_guard {
        client = client.dns_resolver(guard);
    }
    if let Some(guard) = fixed_origin_guard {
        client = client.dns_resolver(guard);
    }
    client.build().map_err(|error| {
        ProviderError::new(
            ProviderErrorKind::Internal,
            format!("could not construct OpenAI HTTP client: {error}"),
        )
    })
}

fn openai_fixed_transport(
    endpoints: &[&str],
    trusted_host: &str,
) -> Result<OpenAiFixedTransport, ProviderError> {
    let guard = Arc::new(FixedOriginGuard::new_allowing(
        endpoints,
        trusted_host,
        Arc::new(SystemFixedDnsResolver),
    )?);
    let client = build_openai_client(
        None,
        Some(Arc::clone(&guard)),
        OPENAI_DEFAULT_TRANSPORT_CONFIG,
    )?;
    Ok(OpenAiFixedTransport { client, guard })
}

fn compatible_transport(
    endpoints: &CompatibleEndpoints,
    policy: CompatibleOriginPolicy,
) -> Result<OpenAiCompatibleTransport, ProviderError> {
    let guard = endpoints.origin.as_ref().map(|origin| {
        Arc::new(CompatibleOriginGuard::new(
            origin.host.clone(),
            origin.port,
            origin.plain_http,
            policy,
            Arc::new(SystemFixedDnsResolver),
        ))
    });
    let client = build_openai_client(guard.clone(), None, OPENAI_DEFAULT_TRANSPORT_CONFIG)?;
    Ok(OpenAiCompatibleTransport { client, guard })
}

pub(crate) async fn openai_compatible_catalog_transport(
    base_url: &str,
    policy: CompatibleOriginPolicy,
    timeout: Duration,
    resolver: Arc<dyn FixedDnsResolver>,
) -> Result<OpenAiCompatibleCatalogTransport, ProviderError> {
    let endpoints = compatible_endpoints(base_url, policy)?;
    let guard = endpoints.origin.as_ref().map(|origin| {
        Arc::new(CompatibleOriginGuard::new(
            origin.host.clone(),
            origin.port,
            origin.plain_http,
            policy,
            resolver,
        ))
    });
    if let Some(guard) = &guard {
        connect_before_deadline(
            OPENAI_DEFAULT_TRANSPORT_CONFIG.connect_timeout,
            guard.validate(),
        )
        .await?;
    }
    let mut client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(OPENAI_DEFAULT_TRANSPORT_CONFIG.connect_timeout)
        .timeout(timeout);
    if let Some(guard) = guard {
        client = client.dns_resolver(guard);
    }
    let client = client.build().map_err(|error| {
        ProviderError::new(
            ProviderErrorKind::Internal,
            format!("could not construct OpenAI-compatible catalog client: {error}"),
        )
    })?;
    Ok(OpenAiCompatibleCatalogTransport {
        client,
        models_url: endpoints.models_url,
    })
}

/// Process-lifetime construction counter used by performance regression
/// harnesses. Production account resolution reuses the complete adapter;
/// standalone constructors deliberately own a configuration-local client.
#[doc(hidden)]
#[must_use]
pub fn openai_http_client_build_count() -> usize {
    OPENAI_CLIENT_BUILD_COUNT.load(Ordering::Relaxed)
}

/// Retry behavior owned by the OpenAI HTTP adapters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiRetryPolicy {
    /// Surface each transport failure once; the actor owns retry/backoff.
    Never,
}

/// Typed per-profile transport budgets shared by native and compatible
/// adapters. [`OpenAiProvider::with_transport_config`] and
/// [`OpenAiCompatibleProvider::with_transport_config`] apply one resolved
/// profile override to every request path owned by that adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenAiTransportConfig {
    pub retry_policy: OpenAiRetryPolicy,
    /// TCP/TLS connection establishment budget (10 seconds by default).
    pub connect_timeout: Duration,
    /// Time from request execution to response headers (60 seconds by
    /// default). The caller's run deadline remains the outer bound.
    pub response_open_timeout: Duration,
    /// Maximum silence between response-body chunks (90 seconds by default).
    pub chunk_idle_timeout: Duration,
}

impl Default for OpenAiTransportConfig {
    fn default() -> Self {
        OPENAI_DEFAULT_TRANSPORT_CONFIG
    }
}

/// Raw response returned only to explicit fixture-promotion harnesses.
#[derive(Debug)]
pub struct OpenAiCapture {
    pub status: u16,
    pub retry_after: Option<String>,
    pub body: Vec<u8>,
}

/// How the credential rides the request (G4b, the `AnthropicAuthMode`
/// pattern): the OpenAI family default is `Authorization: Bearer`; Azure
/// OpenAI's v1 surface authenticates API keys with a bare `api-key` header
/// and NO Authorization header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenAiAuthHeaderMode {
    Bearer,
    AzureApiKey,
    None,
}

#[derive(Debug)]
struct OpenAiHttp {
    client: reqwest::Client,
    credential: SecretHandle,
    account: Option<CredentialAlias>,
    model: String,
    origin_guard: Option<Arc<CompatibleOriginGuard>>,
    fixed_origin_guard: Option<Arc<FixedOriginGuard>>,
    codex_responses_lite: bool,
    auth_header_mode: OpenAiAuthHeaderMode,
    grok_subscription_headers: bool,
    transport_config: OpenAiTransportConfig,
}

impl OpenAiHttp {
    fn new(credential: SecretHandle, model: impl Into<String>) -> Result<Self, ProviderError> {
        Self::new_with_origin_guards(credential, model, None, None, false)
    }

    #[cfg(test)]
    fn new_with_origin_guard(
        credential: SecretHandle,
        model: impl Into<String>,
        origin_guard: Option<Arc<CompatibleOriginGuard>>,
    ) -> Result<Self, ProviderError> {
        Self::new_with_origin_guards(credential, model, origin_guard, None, false)
    }

    fn new_with_shared_origin_transport(
        credential: SecretHandle,
        model: impl Into<String>,
        transport: OpenAiCompatibleTransport,
    ) -> Self {
        Self {
            client: transport.client,
            credential,
            account: None,
            model: model.into(),
            origin_guard: transport.guard,
            fixed_origin_guard: None,
            codex_responses_lite: false,
            auth_header_mode: OpenAiAuthHeaderMode::Bearer,
            grok_subscription_headers: false,
            transport_config: OPENAI_DEFAULT_TRANSPORT_CONFIG,
        }
    }

    #[cfg(test)]
    fn new_subscription(
        credential: SecretHandle,
        model: impl Into<String>,
        endpoint: &str,
        resolver: Arc<dyn FixedDnsResolver>,
    ) -> Result<Self, ProviderError> {
        Self::new_fixed_origin(
            credential,
            model,
            endpoint,
            OPENAI_SUBSCRIPTION_HOST,
            resolver,
            true,
        )
    }

    #[cfg(test)]
    fn new_fixed_origin(
        credential: SecretHandle,
        model: impl Into<String>,
        endpoint: &str,
        trusted_host: &str,
        resolver: Arc<dyn FixedDnsResolver>,
        codex_responses_lite: bool,
    ) -> Result<Self, ProviderError> {
        Self::new_fixed_origins(
            credential,
            model,
            &[endpoint],
            trusted_host,
            resolver,
            codex_responses_lite,
        )
    }

    #[cfg(test)]
    fn new_fixed_origins(
        credential: SecretHandle,
        model: impl Into<String>,
        endpoints: &[&str],
        trusted_host: &str,
        resolver: Arc<dyn FixedDnsResolver>,
        codex_responses_lite: bool,
    ) -> Result<Self, ProviderError> {
        let fixed_origin_guard = Arc::new(FixedOriginGuard::new_allowing(
            endpoints,
            trusted_host,
            resolver,
        )?);
        Self::new_with_origin_guards(
            credential,
            model,
            None,
            Some(fixed_origin_guard),
            codex_responses_lite,
        )
    }

    fn new_fixed_origins_shared_system(
        credential: SecretHandle,
        model: impl Into<String>,
        endpoints: &[&str],
        trusted_host: &str,
        codex_responses_lite: bool,
    ) -> Result<Self, ProviderError> {
        let OpenAiFixedTransport { client, guard } =
            openai_fixed_transport(endpoints, trusted_host)?;
        Ok(Self {
            client,
            credential,
            account: None,
            model: model.into(),
            origin_guard: None,
            fixed_origin_guard: Some(guard),
            codex_responses_lite,
            auth_header_mode: OpenAiAuthHeaderMode::Bearer,
            grok_subscription_headers: false,
            transport_config: OPENAI_DEFAULT_TRANSPORT_CONFIG,
        })
    }

    fn new_with_origin_guards(
        credential: SecretHandle,
        model: impl Into<String>,
        origin_guard: Option<Arc<CompatibleOriginGuard>>,
        fixed_origin_guard: Option<Arc<FixedOriginGuard>>,
        codex_responses_lite: bool,
    ) -> Result<Self, ProviderError> {
        let client = if origin_guard.is_none() && fixed_origin_guard.is_none() {
            build_openai_client(None, None, OPENAI_DEFAULT_TRANSPORT_CONFIG)?
        } else {
            build_openai_client(
                origin_guard.clone(),
                fixed_origin_guard.clone(),
                OPENAI_DEFAULT_TRANSPORT_CONFIG,
            )?
        };
        Ok(Self {
            client,
            credential,
            account: None,
            model: model.into(),
            origin_guard,
            fixed_origin_guard,
            codex_responses_lite,
            auth_header_mode: OpenAiAuthHeaderMode::Bearer,
            grok_subscription_headers: false,
            transport_config: OPENAI_DEFAULT_TRANSPORT_CONFIG,
        })
    }

    fn validate_model(&self, request: &TurnRequest) -> Result<(), ProviderError> {
        if request.model == self.model {
            Ok(())
        } else {
            Err(invalid_request(format!(
                "OpenAI provider selected model `{}`, but turn requested `{}`",
                self.model, request.model
            )))
        }
    }

    fn authorization_header(&self) -> Result<HeaderValue, ProviderError> {
        let secret = self.credential.expose_secret();
        let mut bytes = Vec::with_capacity(7 + secret.len());
        bytes.extend_from_slice(b"Bearer ");
        bytes.extend_from_slice(secret);
        let result = HeaderValue::from_bytes(&bytes).map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::Authentication,
                "resolved OpenAI credential is not a valid HTTP header value",
            )
        });
        bytes.fill(0);
        let mut value = result?;
        value.set_sensitive(true);
        Ok(value)
    }

    /// The bare `api-key` header value for [`OpenAiAuthHeaderMode::AzureApiKey`].
    fn azure_api_key_header(&self) -> Result<HeaderValue, ProviderError> {
        let mut value = HeaderValue::from_bytes(self.credential.expose_secret()).map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::Authentication,
                "resolved Azure OpenAI credential is not a valid HTTP header value",
            )
        })?;
        value.set_sensitive(true);
        Ok(value)
    }

    /// Applies the mode's ONE auth header (G4b, LZ1): Bearer requests carry
    /// `Authorization` and never `api-key`; Azure requests carry `api-key`
    /// and never `Authorization`.
    fn with_auth_header(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder, ProviderError> {
        Ok(match self.auth_header_mode {
            OpenAiAuthHeaderMode::Bearer => {
                request.header(AUTHORIZATION, self.authorization_header()?)
            }
            OpenAiAuthHeaderMode::AzureApiKey => {
                request.header("api-key", self.azure_api_key_header()?)
            }
            OpenAiAuthHeaderMode::None => request,
        })
    }

    async fn post_json_body(
        &self,
        url: &str,
        body: Vec<u8>,
    ) -> Result<reqwest::Response, ProviderError> {
        let request = connect_before_deadline(
            self.transport_config.connect_timeout,
            self.post_json_body_request(url, body),
        )
        .await?;
        let opening = async {
            self.client
                .execute(request)
                .await
                .map_err(|error| transport_error_with_config(error, self.transport_config))
        };
        response_before_deadline(self.transport_config.response_open_timeout, opening).await
    }

    async fn post_json_body_request(
        &self,
        url: &str,
        body: Vec<u8>,
    ) -> Result<reqwest::Request, ProviderError> {
        self.validate_origin(url).await?;
        let request = self.json_request_builder(url)?.body(body);
        request.build().map_err(transport_error)
    }

    fn json_request_builder(&self, url: &str) -> Result<reqwest::RequestBuilder, ProviderError> {
        let mut request = self.with_auth_header(
            self.client
                .post(url)
                .header(CONTENT_TYPE, "application/json")
                .header(ACCEPT, "text/event-stream"),
        )?;
        if self.codex_responses_lite {
            request = request.header(
                OPENAI_CODEX_RESPONSES_LITE_HEADER,
                OPENAI_CODEX_RESPONSES_LITE_VALUE,
            );
        }
        if self.grok_subscription_headers {
            request = apply_grok_subscription_headers(request, Some(&self.model));
        }
        Ok(request)
    }

    async fn get(&self, url: &str) -> Result<reqwest::Response, ProviderError> {
        let request =
            connect_before_deadline(self.transport_config.connect_timeout, self.get_request(url))
                .await?;
        let opening = async {
            self.client
                .execute(request)
                .await
                .map_err(|error| transport_error_with_config(error, self.transport_config))
        };
        response_before_deadline(self.transport_config.response_open_timeout, opening).await
    }

    async fn get_request(&self, url: &str) -> Result<reqwest::Request, ProviderError> {
        self.validate_origin(url).await?;
        let request =
            self.with_auth_header(self.client.get(url).header(ACCEPT, "application/json"))?;
        let request = if self.grok_subscription_headers {
            apply_grok_subscription_headers(request, None)
        } else {
            request
        };
        request.build().map_err(transport_error)
    }

    async fn validate_origin(&self, url: &str) -> Result<(), ProviderError> {
        if let Some(guard) = &self.origin_guard {
            guard.validate().await?;
        }
        if let Some(guard) = &self.fixed_origin_guard {
            guard.validate_endpoint(url).await?;
        }
        Ok(())
    }

    fn set_transport_config(
        &mut self,
        transport_config: OpenAiTransportConfig,
    ) -> Result<(), ProviderError> {
        validate_transport_config(transport_config)?;
        if self.transport_config == transport_config {
            return Ok(());
        }
        self.client = build_openai_client(
            self.origin_guard.clone(),
            self.fixed_origin_guard.clone(),
            transport_config,
        )?;
        self.transport_config = transport_config;
        Ok(())
    }

    #[cfg(test)]
    async fn validate_compatible_origin(&self) -> Result<(), ProviderError> {
        match &self.origin_guard {
            Some(guard) => guard.validate().await,
            None => Ok(()),
        }
    }
}

pub(crate) fn apply_grok_subscription_headers(
    request: reqwest::RequestBuilder,
    model: Option<&str>,
) -> reqwest::RequestBuilder {
    let platform = if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    };
    let request = request
        .header(
            reqwest::header::USER_AGENT,
            format!("grok-shell/{} ({platform})", grok_client_version()),
        )
        .header("x-grok-client-identifier", GROK_SHELL_CLIENT_IDENTIFIER)
        .header("x-grok-client-version", grok_client_version())
        .header("x-grok-client-mode", GROK_SHELL_CLIENT_MODE)
        .header("X-XAI-Token-Auth", GROK_XAI_TOKEN_AUTH);
    match model {
        Some(model) => request.header("x-grok-model-override", model),
        None => request,
    }
}

/// OpenAI-native adapter using `POST /v1/responses`.
#[derive(Debug)]
pub struct OpenAiProvider {
    http: OpenAiHttp,
    api_url: String,
    /// Session-selected reasoning effort (G3), MERGED into the existing
    /// `reasoning` object for reasoning models — never replacing
    /// `summary`/`context`, never adding `max_output_tokens` on lite.
    effort: Option<String>,
    /// W-B: declare the HOSTED `web_search` tool. Structurally inert on the
    /// responses-lite surface (LW4): lite REJECTS every hosted tool, so the
    /// request builder drops it there regardless of this flag.
    web_search: bool,
}

impl OpenAiProvider {
    pub fn new(credential: SecretHandle, model: impl Into<String>) -> Result<Self, ProviderError> {
        Ok(Self {
            http: OpenAiHttp::new(credential, model)?,
            api_url: OPENAI_RESPONSES_API_URL.into(),
            effort: None,
            web_search: false,
        })
    }

    pub fn new_subscription(
        credential: SecretHandle,
        model: impl Into<String>,
        base_url: &str,
    ) -> Result<Self, ProviderError> {
        if base_url != OPENAI_SUBSCRIPTION_BASE_URL {
            return Err(invalid_request(
                "OpenAI subscription inference base URL is not sanctioned",
            ));
        }
        Ok(Self {
            http: OpenAiHttp::new_fixed_origins_shared_system(
                credential,
                model,
                &[OPENAI_SUBSCRIPTION_RESPONSES_URL],
                OPENAI_SUBSCRIPTION_HOST,
                true,
            )?,
            api_url: OPENAI_SUBSCRIPTION_RESPONSES_URL.into(),
            effort: None,
            web_search: false,
        })
    }

    #[cfg(test)]
    fn new_subscription_with_dns_resolver(
        credential: SecretHandle,
        model: impl Into<String>,
        base_url: &str,
        resolver: Arc<dyn FixedDnsResolver>,
    ) -> Result<Self, ProviderError> {
        if base_url != OPENAI_SUBSCRIPTION_BASE_URL {
            return Err(invalid_request(
                "OpenAI subscription inference base URL is not sanctioned",
            ));
        }
        Ok(Self {
            http: OpenAiHttp::new_subscription(
                credential,
                model,
                OPENAI_SUBSCRIPTION_RESPONSES_URL,
                resolver,
            )?,
            api_url: OPENAI_SUBSCRIPTION_RESPONSES_URL.into(),
            effort: None,
            web_search: false,
        })
    }

    #[must_use]
    pub const fn transport_config() -> OpenAiTransportConfig {
        OPENAI_DEFAULT_TRANSPORT_CONFIG
    }

    /// Overrides transport budgets for this resolved provider profile. The
    /// caller's run deadline remains the outer bound and may cancel sooner.
    pub fn with_transport_config(
        mut self,
        transport_config: OpenAiTransportConfig,
    ) -> Result<Self, ProviderError> {
        self.http.set_transport_config(transport_config)?;
        Ok(self)
    }

    #[must_use]
    pub fn with_account(mut self, account: CredentialAlias) -> Self {
        self.http.account = Some(account);
        self
    }

    /// Sets the session-selected reasoning effort merged into `reasoning`.
    #[must_use]
    pub fn with_effort(mut self, effort: Option<String>) -> Self {
        self.effort = effort;
        self
    }

    /// Declares the hosted `web_search` tool (W-B). The caller gates this
    /// per resolved pair; the lite request builder structurally ignores it.
    #[must_use]
    pub fn with_web_search(mut self, web_search: bool) -> Self {
        self.web_search = web_search;
        self
    }

    /// Overrides the endpoint for an explicit capture/test harness.
    #[must_use]
    pub fn with_api_url(mut self, api_url: impl Into<String>) -> Self {
        self.api_url = api_url.into();
        self
    }

    pub fn request_payload(
        &self,
        request: &TurnRequest,
    ) -> Result<serde_json::Value, ProviderError> {
        self.http.validate_model(request)?;
        responses_request_json(
            request,
            self.http.codex_responses_lite,
            self.effort.as_deref(),
            self.web_search,
        )
    }

    pub async fn capture_response(
        &self,
        request: &TurnRequest,
    ) -> Result<OpenAiCapture, ProviderError> {
        let response = self.send_request(request).await?;
        capture(response).await
    }

    async fn send_request(
        &self,
        request: &TurnRequest,
    ) -> Result<reqwest::Response, ProviderError> {
        let mut payload = match crate::take_prepared_wire_payload() {
            Some(prepared) => prepared.payload,
            None => self.request_payload(request)?,
        };
        refresh_openai_cache_routing(request, self.http.codex_responses_lite, &mut payload, None);
        let body = crate::serialize_json_body(payload)?;
        self.http.post_json_body(&self.api_url, body).await
    }

    async fn stream_turn_ref(
        &self,
        request: &TurnRequest,
    ) -> Result<ProviderStream, ProviderError> {
        let computer_kind =
            openai_computer_tool_kind(&request.model, self.http.codex_responses_lite);
        let response = self.send_request(request).await?;
        checked_stream(
            response,
            self.http.account.clone(),
            self.http.transport_config.chunk_idle_timeout,
            DecoderKind::Responses(computer_kind),
        )
        .await
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn credential_surface(&self) -> crate::ProviderCredentialSurface {
        if self.http.codex_responses_lite {
            crate::ProviderCredentialSurface::OAuthSubscriptionBearer
        } else {
            crate::ProviderCredentialSurface::ApiKey
        }
    }

    fn usage_lane_dimensions(&self) -> haider_protocol::provider::UsageLaneDimensions {
        haider_protocol::provider::UsageLaneDimensions {
            api_family: Some("openai_responses".into()),
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
        let boundary = request.cache_metadata.as_ref()?.cacheable_history_end();
        self.http.validate_model(request).ok()?;
        let rendered = responses_request_json_neutral_with_boundary(
            request,
            tools,
            self.http.codex_responses_lite,
            self.effort.as_deref(),
            self.web_search,
            boundary,
        )
        .ok()?;
        let stable_wire_end = rendered.stable_wire_end;
        let previous_wire_end = rendered.previous_wire_end;
        let message_wire_ends = rendered.message_wire_ends;
        let mut full_payload = rendered.payload;
        let rendered_system = if self.http.codex_responses_lite {
            request.system_prompt.as_ref().and_then(|_| {
                full_payload
                    .get("input")
                    .and_then(serde_json::Value::as_array)
                    .and_then(|input| input.first())
            })
        } else {
            full_payload.get("instructions")
        };
        let metadata = request.cache_metadata.as_ref()?;
        let breakpoint_plan = crate::plan_inline_breakpoints(
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
        let history_wire_start =
            usize::from(self.http.codex_responses_lite && request.system_prompt.is_some());
        let history = full_payload.get("input")?.as_array()?;
        let (history_blocks, previous_history_blocks, previous_history_block_len) =
            crate::serialized_provider_view_history(
                history,
                history_wire_start,
                stable_wire_end,
                previous_wire_end,
            )?;
        let mut provider_view = crate::cachemaxxing::prepared_serialized_provider_view(
            request,
            if self.http.codex_responses_lite {
                "openai_responses_lite"
            } else {
                "openai_responses"
            },
            rendered_system,
            full_payload.get("tools"),
            history_blocks,
            previous_history_blocks,
            breakpoint_plan.ledger_boundaries(),
        )?;
        let (prefix_digests, mut previous_immutable_history_digest, provider_view_storage_blobs) =
            crate::rendered_prefix_digests_from_provider_view(
                request,
                &mut provider_view,
                self.http.codex_responses_lite && request.system_prompt.is_some(),
                previous_history_block_len,
            )?;
        if previous_immutable_history_digest.is_none() {
            previous_immutable_history_digest = previous_wire_end.map(|end| {
                crate::exact_optional_wire_digest(Some(&history[..end.min(history.len())]))
            });
        }
        let header_epoch = provider_view.ledger().header_epoch.as_str();
        apply_openai_cache_controls(
            request,
            self.http.codex_responses_lite,
            &mut full_payload,
            &message_wire_ends,
            boundary,
            !tools.is_empty(),
            Some(header_epoch),
        );
        let cache_control = openai_cache_control_observation(request, &full_payload);
        Some(crate::PreparedTurn {
            prefix_digests,
            previous_immutable_history_digest,
            cache_control,
            provider_view: Some(provider_view),
            provider_view_storage_blobs,
            wire: Some(crate::PreparedWire {
                payload: full_payload,
                history_boundary: None,
            }),
        })
    }

    async fn prewarm(&self) {
        crate::optional_http_prewarm(&self.http.client, &self.api_url).await;
    }

    async fn capabilities(&self) -> CapabilityDoc {
        native_capabilities(&self.http.model)
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

/// How strictly an OpenAI-compatible origin is fenced (G4a).
///
/// `Strict` is the release-owned default: private, link-local, and
/// special-use targets are refused entirely and plain HTTP is loopback-only.
/// `TrustedLan` is the DELIBERATE, SCOPED loosening for CUSTOM-provenance
/// profiles only: RFC1918 private ranges (10/8, 172.16/12, 192.168/16)
/// become valid credential targets over http AND https — a LAN Ollama or
/// LM Studio box — while link-local `169.254.0.0/16` (cloud metadata),
/// multicast, unspecified/broadcast, IPv6 ULA/link-local, and public
/// plain-HTTP origins stay refused. Builtin providers never construct with
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompatibleOriginPolicy {
    #[default]
    Strict,
    TrustedLan,
}

/// One generic OpenAI-compatible adapter parameterized by a credential base
/// URL. It deliberately uses Chat Completions rather than assuming that a
/// third-party endpoint implements the newer Responses API.
#[derive(Debug)]
pub struct OpenAiCompatibleProvider {
    http: OpenAiHttp,
    base_url: String,
    chat_url: String,
    models_url: String,
    dialect: CompatibleDialect,
    /// Provider-declared facts from the daemon's durable catalog cache for
    /// this already-pinned model. Turn startup must never refetch them.
    catalog_model: Option<crate::DiscoveredModel>,
    kimi_thinking: Option<KimiThinkingConfig>,
    /// Kimi k3-style top-level `reasoning_effort` (G3): always-thinking
    /// models whose catalog declares an effort ladder but NO thinking-type
    /// toggle take the documented top-level knob instead of
    /// `thinking.effort`.
    kimi_reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompatibleDialect {
    Generic,
    KimiOAuth,
    DeepSeekApi,
    HaiderCodeApi,
    XaiApi,
    GrokOAuth,
}

impl CompatibleDialect {
    const fn provider_view_name(self) -> &'static str {
        match self {
            Self::Generic => "openai_chat_generic",
            Self::KimiOAuth => "openai_chat_kimi_oauth",
            Self::DeepSeekApi => "openai_chat_deepseek",
            Self::HaiderCodeApi => "openai_chat_haider_code",
            Self::XaiApi => "openai_chat_xai",
            Self::GrokOAuth => "openai_chat_grok_oauth",
        }
    }
}

/// Kimi's opt-in `extra_body.thinking` request extension.
///
/// The Kimi adapter defaults to `None`, so thinking remains off and the
/// generic OpenAI-compatible payload stays byte-identical.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct KimiThinkingConfig {
    #[serde(rename = "type")]
    pub thinking_type: KimiThinkingType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keep: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KimiThinkingType {
    Enabled,
    Disabled,
}

impl OpenAiCompatibleProvider {
    pub fn new(
        credential: SecretHandle,
        model: impl Into<String>,
        base_url: impl AsRef<str>,
    ) -> Result<Self, ProviderError> {
        Self::new_with_policy_shared(credential, model, base_url, CompatibleOriginPolicy::Strict)
    }

    /// Constructs the CUSTOM-provenance adapter under
    /// [`CompatibleOriginPolicy::TrustedLan`] (G4a): RFC1918 LAN origins are
    /// valid over http and https; the rest of the origin fence is unchanged.
    pub fn new_custom(
        credential: SecretHandle,
        model: impl Into<String>,
        base_url: impl AsRef<str>,
    ) -> Result<Self, ProviderError> {
        Self::new_with_policy_shared(
            credential,
            model,
            base_url,
            CompatibleOriginPolicy::TrustedLan,
        )
    }

    /// Constructs a custom profile that sends no authentication header.
    /// The handle remains an internal construction token only; its bytes are
    /// never placed on the wire.
    pub fn new_custom_no_auth(
        credential: SecretHandle,
        model: impl Into<String>,
        base_url: impl AsRef<str>,
    ) -> Result<Self, ProviderError> {
        let mut provider = Self::new_with_policy_shared(
            credential,
            model,
            base_url,
            CompatibleOriginPolicy::TrustedLan,
        )?;
        provider.http.auth_header_mode = OpenAiAuthHeaderMode::None;
        Ok(provider)
    }

    /// Constructs the Azure OpenAI v1 adapter (G4b, LZ1): the SAME Chat
    /// Completions wire under the STRICT origin fence (Azure endpoints are
    /// public HTTPS only), with the credential riding the bare `api-key`
    /// header instead of `Authorization: Bearer`. Refuses any origin that
    /// is not an Azure OpenAI resource host so the header mode can never
    /// leak a key to an arbitrary endpoint.
    pub fn new_azure(
        credential: SecretHandle,
        model: impl Into<String>,
        base_url: impl AsRef<str>,
    ) -> Result<Self, ProviderError> {
        if !azure_openai_origin(base_url.as_ref()) {
            return Err(invalid_request(
                "Azure OpenAI endpoints must be https on *.openai.azure.com or *.services.ai.azure.com",
            ));
        }
        let mut provider = Self::new_with_policy_shared(
            credential,
            model,
            base_url,
            CompatibleOriginPolicy::Strict,
        )?;
        provider.http.auth_header_mode = OpenAiAuthHeaderMode::AzureApiKey;
        Ok(provider)
    }

    #[cfg(test)]
    fn new_azure_with_dns_resolver(
        credential: SecretHandle,
        model: impl Into<String>,
        base_url: impl AsRef<str>,
        resolver: Arc<dyn FixedDnsResolver>,
    ) -> Result<Self, ProviderError> {
        if !azure_openai_origin(base_url.as_ref()) {
            return Err(invalid_request(
                "Azure OpenAI endpoints must be https on *.openai.azure.com or *.services.ai.azure.com",
            ));
        }
        let mut provider = Self::new_with_policy_and_dns_resolver(
            credential,
            model,
            base_url,
            CompatibleOriginPolicy::Strict,
            resolver,
        )?;
        provider.http.auth_header_mode = OpenAiAuthHeaderMode::AzureApiKey;
        Ok(provider)
    }

    fn new_with_policy_shared(
        credential: SecretHandle,
        model: impl Into<String>,
        base_url: impl AsRef<str>,
        policy: CompatibleOriginPolicy,
    ) -> Result<Self, ProviderError> {
        let endpoints = compatible_endpoints(base_url.as_ref(), policy)?;
        let transport = compatible_transport(&endpoints, policy)?;
        Ok(Self {
            http: OpenAiHttp::new_with_shared_origin_transport(credential, model, transport),
            base_url: endpoints.base_url,
            chat_url: endpoints.chat_url,
            models_url: endpoints.models_url,
            dialect: CompatibleDialect::Generic,
            catalog_model: None,
            kimi_thinking: None,
            kimi_reasoning_effort: None,
        })
    }

    #[cfg(test)]
    fn new_with_dns_resolver(
        credential: SecretHandle,
        model: impl Into<String>,
        base_url: impl AsRef<str>,
        resolver: Arc<dyn FixedDnsResolver>,
    ) -> Result<Self, ProviderError> {
        Self::new_with_policy_and_dns_resolver(
            credential,
            model,
            base_url,
            CompatibleOriginPolicy::Strict,
            resolver,
        )
    }

    #[cfg(test)]
    fn new_with_policy_and_dns_resolver(
        credential: SecretHandle,
        model: impl Into<String>,
        base_url: impl AsRef<str>,
        policy: CompatibleOriginPolicy,
        resolver: Arc<dyn FixedDnsResolver>,
    ) -> Result<Self, ProviderError> {
        let endpoints = compatible_endpoints(base_url.as_ref(), policy)?;
        let origin_guard = endpoints.origin.map(|origin| {
            Arc::new(CompatibleOriginGuard::new(
                origin.host,
                origin.port,
                origin.plain_http,
                policy,
                resolver,
            ))
        });
        Ok(Self {
            http: OpenAiHttp::new_with_origin_guard(credential, model, origin_guard)?,
            base_url: endpoints.base_url,
            chat_url: endpoints.chat_url,
            models_url: endpoints.models_url,
            dialect: CompatibleDialect::Generic,
            catalog_model: None,
            kimi_thinking: None,
            kimi_reasoning_effort: None,
        })
    }

    /// Constructs the release-owned Kimi OAuth Chat Completions dialect.
    pub fn new_kimi_subscription(
        credential: SecretHandle,
        model: impl Into<String>,
        base_url: &str,
    ) -> Result<Self, ProviderError> {
        if base_url != KIMI_OAUTH_BASE_URL {
            return Err(invalid_request(
                "Kimi OAuth inference base URL is not sanctioned",
            ));
        }
        let endpoints = compatible_endpoints(base_url, CompatibleOriginPolicy::Strict)?;
        let http = OpenAiHttp::new_fixed_origins_shared_system(
            credential,
            model,
            &[&endpoints.chat_url, &endpoints.models_url],
            KIMI_OAUTH_HOST,
            false,
        )?;
        Ok(Self {
            http,
            base_url: endpoints.base_url,
            chat_url: endpoints.chat_url,
            models_url: endpoints.models_url,
            dialect: CompatibleDialect::KimiOAuth,
            catalog_model: None,
            kimi_thinking: None,
            kimi_reasoning_effort: None,
        })
    }

    #[cfg(test)]
    fn new_kimi_subscription_with_dns_resolver(
        credential: SecretHandle,
        model: impl Into<String>,
        base_url: &str,
        resolver: Arc<dyn FixedDnsResolver>,
    ) -> Result<Self, ProviderError> {
        if base_url != KIMI_OAUTH_BASE_URL {
            return Err(invalid_request(
                "Kimi OAuth inference base URL is not sanctioned",
            ));
        }
        let endpoints = compatible_endpoints(base_url, CompatibleOriginPolicy::Strict)?;
        let http = OpenAiHttp::new_fixed_origins(
            credential,
            model,
            &[&endpoints.chat_url, &endpoints.models_url],
            KIMI_OAUTH_HOST,
            resolver,
            false,
        )?;
        Ok(Self {
            http,
            base_url: endpoints.base_url,
            chat_url: endpoints.chat_url,
            models_url: endpoints.models_url,
            dialect: CompatibleDialect::KimiOAuth,
            catalog_model: None,
            kimi_thinking: None,
            kimi_reasoning_effort: None,
        })
    }

    /// Constructs the release-owned DeepSeek API-key Chat Completions
    /// dialect. Unlike a custom compatible profile, this is fixed to the
    /// vendor's root `/chat/completions` and `/models` paths.
    pub fn new_deepseek_api(
        credential: SecretHandle,
        model: impl Into<String>,
        base_url: &str,
    ) -> Result<Self, ProviderError> {
        if base_url != DEEPSEEK_BASE_URL {
            return Err(invalid_request(
                "DeepSeek inference base URL is not sanctioned",
            ));
        }
        let chat_url = format!("{base_url}/chat/completions");
        let models_url = format!("{base_url}/models");
        let http = OpenAiHttp::new_fixed_origins_shared_system(
            credential,
            model,
            &[&chat_url, &models_url],
            DEEPSEEK_HOST,
            false,
        )?;
        Ok(Self {
            http,
            base_url: base_url.to_owned(),
            chat_url,
            models_url,
            dialect: CompatibleDialect::DeepSeekApi,
            catalog_model: None,
            kimi_thinking: None,
            kimi_reasoning_effort: None,
        })
    }

    #[cfg(test)]
    fn new_deepseek_api_with_dns_resolver(
        credential: SecretHandle,
        model: impl Into<String>,
        base_url: &str,
        resolver: Arc<dyn FixedDnsResolver>,
    ) -> Result<Self, ProviderError> {
        if base_url != DEEPSEEK_BASE_URL {
            return Err(invalid_request(
                "DeepSeek inference base URL is not sanctioned",
            ));
        }
        let chat_url = format!("{base_url}/chat/completions");
        let models_url = format!("{base_url}/models");
        let http = OpenAiHttp::new_fixed_origins(
            credential,
            model,
            &[&chat_url, &models_url],
            DEEPSEEK_HOST,
            resolver,
            false,
        )?;
        Ok(Self {
            http,
            base_url: base_url.to_owned(),
            chat_url,
            models_url,
            dialect: CompatibleDialect::DeepSeekApi,
            catalog_model: None,
            kimi_thinking: None,
            kimi_reasoning_effort: None,
        })
    }

    /// Constructs xAI's fixed API-key Chat Completions adapter.
    pub fn new_xai_api(
        credential: SecretHandle,
        model: impl Into<String>,
        base_url: &str,
    ) -> Result<Self, ProviderError> {
        if base_url != XAI_BASE_URL {
            return Err(invalid_request("xAI inference base URL is not sanctioned"));
        }
        Self::new_fixed_builtin(
            credential,
            model,
            base_url,
            XAI_HOST,
            CompatibleDialect::XaiApi,
        )
    }

    #[cfg(test)]
    fn new_xai_api_with_dns_resolver(
        credential: SecretHandle,
        model: impl Into<String>,
        base_url: &str,
        resolver: Arc<dyn FixedDnsResolver>,
    ) -> Result<Self, ProviderError> {
        if base_url != XAI_BASE_URL {
            return Err(invalid_request("xAI inference base URL is not sanctioned"));
        }
        let endpoints = compatible_endpoints(base_url, CompatibleOriginPolicy::Strict)?;
        let http = OpenAiHttp::new_fixed_origins(
            credential,
            model,
            &[&endpoints.chat_url, &endpoints.models_url],
            XAI_HOST,
            resolver,
            false,
        )?;
        Ok(Self {
            http,
            base_url: endpoints.base_url,
            chat_url: endpoints.chat_url,
            models_url: endpoints.models_url,
            dialect: CompatibleDialect::XaiApi,
            catalog_model: None,
            kimi_thinking: None,
            kimi_reasoning_effort: None,
        })
    }

    /// Constructs Haider Code's fixed API-key Chat Completions adapter.
    pub fn new_haider_code_api(
        credential: SecretHandle,
        model: impl Into<String>,
        base_url: &str,
    ) -> Result<Self, ProviderError> {
        if base_url != HAIDER_CODE_BASE_URL {
            return Err(invalid_request(
                "Haider Code inference base URL is not sanctioned",
            ));
        }
        Self::new_fixed_builtin(
            credential,
            model,
            base_url,
            HAIDER_CODE_HOST,
            CompatibleDialect::HaiderCodeApi,
        )
    }

    #[cfg(test)]
    fn new_haider_code_api_with_dns_resolver(
        credential: SecretHandle,
        model: impl Into<String>,
        base_url: &str,
        resolver: Arc<dyn FixedDnsResolver>,
    ) -> Result<Self, ProviderError> {
        if base_url != HAIDER_CODE_BASE_URL {
            return Err(invalid_request(
                "Haider Code inference base URL is not sanctioned",
            ));
        }
        let endpoints = compatible_endpoints(base_url, CompatibleOriginPolicy::Strict)?;
        let http = OpenAiHttp::new_fixed_origins(
            credential,
            model,
            &[&endpoints.chat_url, &endpoints.models_url],
            HAIDER_CODE_HOST,
            resolver,
            false,
        )?;
        Ok(Self {
            http,
            base_url: endpoints.base_url,
            chat_url: endpoints.chat_url,
            models_url: endpoints.models_url,
            dialect: CompatibleDialect::HaiderCodeApi,
            catalog_model: None,
            kimi_thinking: None,
            kimi_reasoning_effort: None,
        })
    }

    /// Constructs the fixed SuperGrok/X Premium subscription proxy adapter.
    pub fn new_grok_subscription(
        credential: SecretHandle,
        model: impl Into<String>,
        base_url: &str,
    ) -> Result<Self, ProviderError> {
        if base_url != GROK_OAUTH_BASE_URL {
            return Err(invalid_request(
                "Grok OAuth inference base URL is not sanctioned",
            ));
        }
        let mut provider = Self::new_fixed_builtin(
            credential,
            model,
            base_url,
            GROK_OAUTH_HOST,
            CompatibleDialect::GrokOAuth,
        )?;
        provider.http.grok_subscription_headers = true;
        Ok(provider)
    }

    fn new_fixed_builtin(
        credential: SecretHandle,
        model: impl Into<String>,
        base_url: &str,
        host: &str,
        dialect: CompatibleDialect,
    ) -> Result<Self, ProviderError> {
        let endpoints = compatible_endpoints(base_url, CompatibleOriginPolicy::Strict)?;
        let http = OpenAiHttp::new_fixed_origins_shared_system(
            credential,
            model,
            &[&endpoints.chat_url, &endpoints.models_url],
            host,
            false,
        )?;
        Ok(Self {
            http,
            base_url: endpoints.base_url,
            chat_url: endpoints.chat_url,
            models_url: endpoints.models_url,
            dialect,
            catalog_model: None,
            kimi_thinking: None,
            kimi_reasoning_effort: None,
        })
    }

    #[cfg(test)]
    fn new_grok_subscription_with_dns_resolver(
        credential: SecretHandle,
        model: impl Into<String>,
        base_url: &str,
        resolver: Arc<dyn FixedDnsResolver>,
    ) -> Result<Self, ProviderError> {
        if base_url != GROK_OAUTH_BASE_URL {
            return Err(invalid_request(
                "Grok OAuth inference base URL is not sanctioned",
            ));
        }
        let endpoints = compatible_endpoints(base_url, CompatibleOriginPolicy::Strict)?;
        let mut http = OpenAiHttp::new_fixed_origins(
            credential,
            model,
            &[&endpoints.chat_url, &endpoints.models_url],
            GROK_OAUTH_HOST,
            resolver,
            false,
        )?;
        http.grok_subscription_headers = true;
        Ok(Self {
            http,
            base_url: endpoints.base_url,
            chat_url: endpoints.chat_url,
            models_url: endpoints.models_url,
            dialect: CompatibleDialect::GrokOAuth,
            catalog_model: None,
            kimi_thinking: None,
            kimi_reasoning_effort: None,
        })
    }

    /// Enables or explicitly disables Kimi thinking passthrough. Generic
    /// compatible adapters reject this provider-specific seam.
    pub fn with_kimi_thinking(
        mut self,
        thinking: KimiThinkingConfig,
    ) -> Result<Self, ProviderError> {
        if self.dialect != CompatibleDialect::KimiOAuth {
            return Err(invalid_request(
                "Kimi thinking options require the Kimi OAuth adapter",
            ));
        }
        self.kimi_thinking = Some(thinking);
        Ok(self)
    }

    /// Sets the k3-style top-level `reasoning_effort` (G3). Kimi-only, like
    /// [`Self::with_kimi_thinking`]; the caller injects only what the
    /// catalog declared for the model.
    pub fn with_kimi_reasoning_effort(
        mut self,
        effort: impl Into<String>,
    ) -> Result<Self, ProviderError> {
        if self.dialect != CompatibleDialect::KimiOAuth {
            return Err(invalid_request(
                "Kimi reasoning_effort requires the Kimi OAuth adapter",
            ));
        }
        self.kimi_reasoning_effort = Some(effort.into());
        Ok(self)
    }

    #[must_use]
    pub const fn transport_config() -> OpenAiTransportConfig {
        OPENAI_DEFAULT_TRANSPORT_CONFIG
    }

    /// Overrides transport budgets for this resolved provider profile. The
    /// same typed field applies to every compatible dialect and endpoint.
    pub fn with_transport_config(
        mut self,
        transport_config: OpenAiTransportConfig,
    ) -> Result<Self, ProviderError> {
        self.http.set_transport_config(transport_config)?;
        Ok(self)
    }

    #[must_use]
    pub fn with_account(mut self, account: CredentialAlias) -> Self {
        self.http.account = Some(account);
        self
    }

    /// Supplies the already-discovered record for this pinned model.
    ///
    /// Kimi and Grok publish capability facts in their catalogs. The daemon
    /// hydrates this record from its durable catalog cache, so ordinary turn
    /// startup can retain those facts without issuing another `/models`
    /// request. A stale/mismatched record is ignored and never substitutes a
    /// different model for the caller's explicit selection.
    #[must_use]
    pub fn with_cached_catalog_model(mut self, model: Option<&crate::DiscoveredModel>) -> Self {
        self.catalog_model = model.filter(|model| model.slug == self.http.model).cloned();
        self
    }

    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    #[must_use]
    pub fn models_url(&self) -> &str {
        &self.models_url
    }

    pub fn request_payload(
        &self,
        request: &TurnRequest,
    ) -> Result<serde_json::Value, ProviderError> {
        self.http.validate_model(request)?;
        chat_request_json(
            request,
            self.dialect,
            self.kimi_thinking.as_ref(),
            self.kimi_reasoning_effort.as_deref(),
        )
    }

    /// Explicitly probes `GET /models` for discovery-backed capability facts.
    ///
    /// Ordinary pinned turns do not use this merely to validate model
    /// membership: the chat request is the authority for an explicitly
    /// selected model. Kimi and Grok receive any richer capability facts
    /// from the daemon's cached catalog through
    /// [`Self::with_cached_catalog_model`].
    pub async fn probe_capabilities(&self) -> Result<CapabilityDoc, ProviderError> {
        let response = self.http.get(&self.models_url).await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let error = http_error_from_response(response).await;
            return Err(if self.dialect == CompatibleDialect::GrokOAuth {
                grok_version_gate_error(status, error)
            } else {
                error
            });
        }
        let body =
            read_body_bounded(response, MODELS_BODY_LIMIT, "OpenAI-compatible /models").await?;
        match self.dialect {
            CompatibleDialect::Generic => replay_openai_models_response(&self.http.model, &body),
            CompatibleDialect::KimiOAuth => replay_kimi_models_response(&self.http.model, &body),
            CompatibleDialect::DeepSeekApi => {
                replay_deepseek_models_response(&self.http.model, &body)
            }
            CompatibleDialect::HaiderCodeApi => {
                replay_haider_code_models_response(&self.http.model, &body)
            }
            CompatibleDialect::XaiApi => replay_xai_models_response(&self.http.model, &body),
            CompatibleDialect::GrokOAuth => replay_grok_models_response(&self.http.model, &body),
        }
    }

    pub async fn capture_response(
        &self,
        request: &TurnRequest,
    ) -> Result<OpenAiCapture, ProviderError> {
        let response = self.send_request(request).await?;
        capture(response).await
    }

    async fn send_request(
        &self,
        request: &TurnRequest,
    ) -> Result<reqwest::Response, ProviderError> {
        let outbound = connect_before_deadline(
            self.http.transport_config.connect_timeout,
            self.inference_request(request),
        )
        .await?;
        let opening = async {
            self.http
                .client
                .execute(outbound)
                .await
                .map_err(|error| transport_error_with_config(error, self.http.transport_config))
        };
        response_before_deadline(self.http.transport_config.response_open_timeout, opening).await
    }

    async fn inference_request(
        &self,
        request: &TurnRequest,
    ) -> Result<reqwest::Request, ProviderError> {
        let mut payload = match crate::take_prepared_wire_payload() {
            Some(prepared) => prepared.payload,
            None => self.request_payload(request)?,
        };
        if matches!(
            self.dialect,
            CompatibleDialect::Generic | CompatibleDialect::KimiOAuth
        ) && let Some(object) = payload.as_object_mut()
        {
            let key = request
                .cache_metadata
                .as_ref()
                .and_then(|metadata| match self.dialect {
                    CompatibleDialect::Generic => custom_prompt_cache_key(request, metadata, None),
                    CompatibleDialect::KimiOAuth
                        if metadata.boundaries_valid(request.messages.len())
                            && metadata.provider == KIMI_OAUTH_PROVIDER_NAME =>
                    {
                        prompt_cache_cohort_key(request, metadata)
                    }
                    CompatibleDialect::KimiOAuth
                    | CompatibleDialect::DeepSeekApi
                    | CompatibleDialect::HaiderCodeApi
                    | CompatibleDialect::XaiApi
                    | CompatibleDialect::GrokOAuth => None,
                });
            if let Some(key) = key {
                object.insert("prompt_cache_key".into(), serde_json::Value::String(key));
            } else {
                object.remove("prompt_cache_key");
            }
        }
        let body = crate::serialize_json_body(payload)?;
        let mut outbound = self
            .http
            .post_json_body_request(&self.chat_url, body)
            .await?;
        if self.dialect == CompatibleDialect::XaiApi
            && let Some(conversation_id) = xai_prompt_cache_conversation_id(request, None)
        {
            outbound.headers_mut().insert(
                XAI_CONVERSATION_ID_HEADER,
                HeaderValue::from_bytes(conversation_id.as_bytes()).map_err(|_| {
                    internal("derived xAI conversation ID was not a valid HTTP header value")
                })?,
            );
        }
        Ok(outbound)
    }

    async fn stream_turn_ref(
        &self,
        request: &TurnRequest,
    ) -> Result<ProviderStream, ProviderError> {
        let response = self.send_request(request).await?;
        checked_stream(
            response,
            self.http.account.clone(),
            self.http.transport_config.chunk_idle_timeout,
            DecoderKind::Chat(self.dialect),
        )
        .await
    }
}

#[async_trait]
impl Provider for OpenAiCompatibleProvider {
    fn credential_surface(&self) -> crate::ProviderCredentialSurface {
        match self.dialect {
            CompatibleDialect::Generic
            | CompatibleDialect::DeepSeekApi
            | CompatibleDialect::HaiderCodeApi
            | CompatibleDialect::XaiApi => crate::ProviderCredentialSurface::ApiKey,
            CompatibleDialect::KimiOAuth | CompatibleDialect::GrokOAuth => {
                crate::ProviderCredentialSurface::OAuthSubscriptionBearer
            }
        }
    }

    fn usage_lane_dimensions(&self) -> haider_protocol::provider::UsageLaneDimensions {
        haider_protocol::provider::UsageLaneDimensions {
            api_family: Some("openai_chat_completions".into()),
            effort: self
                .kimi_thinking
                .as_ref()
                .and_then(|thinking| thinking.effort.clone())
                .or_else(|| self.kimi_reasoning_effort.clone()),
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
        let boundary = request.cache_metadata.as_ref()?.cacheable_history_end();
        self.http.validate_model(request).ok()?;
        let (mut full_payload, stable_wire_end, previous_wire_end) =
            chat_request_json_with_boundary(
                request,
                tools,
                self.dialect,
                self.kimi_thinking.as_ref(),
                self.kimi_reasoning_effort.as_deref(),
                boundary,
            )
            .ok()?;
        // Kimi's cohort key is a routing overlay, not prompt content. Remove
        // it from the only render until M4's exact provider view is frozen.
        if matches!(
            self.dialect,
            CompatibleDialect::Generic | CompatibleDialect::KimiOAuth
        ) && let Some(object) = full_payload.as_object_mut()
        {
            object.remove("prompt_cache_key");
        }
        let rendered_system = full_payload
            .get("messages")
            .and_then(serde_json::Value::as_array)
            .and_then(|messages| messages.first())
            .filter(|message| {
                message.get("role").and_then(serde_json::Value::as_str) == Some("system")
            });
        let history_wire_start = usize::from(request.system_prompt.is_some());
        let history = full_payload.get("messages")?.as_array()?;
        let (history_blocks, previous_history_blocks, previous_history_block_len) =
            crate::serialized_provider_view_history(
                history,
                history_wire_start,
                stable_wire_end,
                previous_wire_end,
            )?;
        let mut provider_view = crate::cachemaxxing::prepared_serialized_provider_view(
            request,
            self.dialect.provider_view_name(),
            rendered_system,
            full_payload.get("tools"),
            history_blocks,
            previous_history_blocks,
            Vec::new(),
        )?;
        let (
            mut prefix_digests,
            mut previous_immutable_history_digest,
            provider_view_storage_blobs,
        ) = crate::rendered_prefix_digests_from_provider_view(
            request,
            &mut provider_view,
            request.system_prompt.is_some(),
            previous_history_block_len,
        )?;
        if previous_immutable_history_digest.is_none() {
            previous_immutable_history_digest = previous_wire_end.map(|end| {
                crate::exact_optional_wire_digest(Some(&history[..end.min(history.len())]))
            });
        }
        if !matches!(
            self.dialect,
            CompatibleDialect::KimiOAuth | CompatibleDialect::XaiApi
        ) {
            // Preserve the established diagnostic ABI for unrelated
            // compatible dialects: their top-level `system` field is absent
            // even though the provider view correctly records message[0].
            prefix_digests.system = crate::exact_optional_wire_digest::<serde_json::Value>(None);
        }
        let header_epoch = provider_view.ledger().header_epoch.as_str();
        if matches!(
            self.dialect,
            CompatibleDialect::Generic | CompatibleDialect::KimiOAuth
        ) && let Some(object) = full_payload.as_object_mut()
        {
            if let Some(key) =
                request
                    .cache_metadata
                    .as_ref()
                    .and_then(|metadata| match self.dialect {
                        CompatibleDialect::Generic => {
                            custom_prompt_cache_key(request, metadata, Some(header_epoch))
                        }
                        CompatibleDialect::KimiOAuth => prompt_cache_cohort_key_with_header(
                            request,
                            metadata,
                            Some(header_epoch),
                        ),
                        CompatibleDialect::DeepSeekApi
                        | CompatibleDialect::HaiderCodeApi
                        | CompatibleDialect::XaiApi
                        | CompatibleDialect::GrokOAuth => None,
                    })
            {
                object.insert("prompt_cache_key".into(), serde_json::Value::String(key));
            } else {
                object.remove("prompt_cache_key");
            }
        }
        let cache_control = compatible_cache_control_observation(
            request,
            &full_payload,
            self.dialect,
            Some(header_epoch),
        );
        Some(crate::PreparedTurn {
            prefix_digests,
            previous_immutable_history_digest,
            cache_control,
            provider_view: Some(provider_view),
            provider_view_storage_blobs,
            wire: Some(crate::PreparedWire {
                payload: full_payload,
                history_boundary: None,
            }),
        })
    }

    async fn prewarm(&self) {
        crate::optional_http_prewarm(&self.http.client, &self.chat_url).await;
    }

    async fn capabilities(&self) -> CapabilityDoc {
        let provider = match self.dialect {
            CompatibleDialect::Generic => OPENAI_COMPATIBLE_PROVIDER_NAME,
            CompatibleDialect::KimiOAuth => KIMI_OAUTH_PROVIDER_NAME,
            CompatibleDialect::DeepSeekApi => DEEPSEEK_PROVIDER_NAME,
            CompatibleDialect::HaiderCodeApi => HAIDER_CODE_PROVIDER_NAME,
            CompatibleDialect::XaiApi => XAI_PROVIDER_NAME,
            CompatibleDialect::GrokOAuth => GROK_OAUTH_PROVIDER_NAME,
        };
        match self.dialect {
            // These catalogs contribute model-specific capability facts, so
            // consume the daemon's typed cache rather than refetching during
            // a pinned turn. Missing/stale facts degrade locally; they never
            // trigger model substitution or request-time discovery.
            CompatibleDialect::KimiOAuth => self
                .catalog_model
                .as_ref()
                .and_then(|model| kimi_capabilities_from_model(model).ok())
                .unwrap_or_else(|| unavailable_compatible_capabilities(provider)),
            CompatibleDialect::GrokOAuth => self
                .catalog_model
                .as_ref()
                .map(grok_capabilities_from_model)
                .unwrap_or_else(|| unavailable_compatible_capabilities(provider)),
            // For every other compatible dialect a successful /models probe
            // returns this exact conservative document and contributes only
            // a membership check. Session models are already pinned before a
            // turn reaches the adapter, so avoid the redundant request and
            // let the chat endpoint report an invalid explicit model.
            CompatibleDialect::Generic
            | CompatibleDialect::DeepSeekApi
            | CompatibleDialect::HaiderCodeApi
            | CompatibleDialect::XaiApi => compatible_capabilities(provider),
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

async fn capture(response: reqwest::Response) -> Result<OpenAiCapture, ProviderError> {
    let status = response.status().as_u16();
    let success = response.status().is_success();
    let retry_after = response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = if success {
        response.bytes().await.map_err(transport_error)?.to_vec()
    } else {
        read_body_bounded(response, crate::HTTP_ERROR_BODY_LIMIT, "OpenAI HTTP error").await?
    };
    Ok(OpenAiCapture {
        status,
        retry_after,
        body,
    })
}

async fn checked_stream(
    response: reqwest::Response,
    account: Option<CredentialAlias>,
    chunk_idle_timeout: Duration,
    decoder: DecoderKind,
) -> Result<ProviderStream, ProviderError> {
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let error = http_error_from_response(response).await;
        return Err(match decoder {
            DecoderKind::Chat(CompatibleDialect::GrokOAuth) => {
                grok_version_gate_error(status, error)
            }
            _ => error,
        });
    }
    let (sender, receiver) = mpsc::channel(STREAM_CAPACITY);
    let producer = tokio::spawn(async move {
        stream_response(response, account, sender, chunk_idle_timeout, decoder).await;
    });
    Ok(ProviderStream::owned(receiver, producer))
}

fn grok_version_gate_error(status: u16, error: ProviderError) -> ProviderError {
    if matches!(status, 402 | 426) {
        ProviderError::new(
            ProviderErrorKind::ConnectionConfiguration,
            format!(
                "Grok subscription proxy rejected grok-shell client version {GROK_SHELL_CLIENT_VERSION} (HTTP {}); update Haider's admitted Grok client version and retry",
                status
            ),
        )
        .with_http_metadata(status, None)
    } else {
        error
    }
}

async fn http_error_from_response(response: reqwest::Response) -> ProviderError {
    let status = response.status().as_u16();
    let request_id = response
        .headers()
        .get("x-request-id")
        .or_else(|| response.headers().get("request-id"))
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let retry_after = response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let error = match read_body_bounded(response, crate::HTTP_ERROR_BODY_LIMIT, "OpenAI HTTP error")
        .await
    {
        Ok(body) => replay_openai_http_error(status, retry_after.as_deref(), &body),
        Err(error) => classify_http_body_read_error(status, retry_after.as_deref(), error),
    };
    error.with_http_metadata(status, request_id.as_deref())
}

fn classify_http_body_read_error(
    status: u16,
    retry_after: Option<&str>,
    mut error: ProviderError,
) -> ProviderError {
    if error.kind == ProviderErrorKind::MalformedFrame {
        let classified = replay_openai_http_error(status, retry_after, &[]);
        error.kind = classified.kind;
        error.retryable = classified.retryable;
        error.retry_after_ms = classified.retry_after_ms;
        error.presentation = classified.presentation;
    }
    error
}

async fn read_body_bounded(
    response: reqwest::Response,
    limit: usize,
    context: &str,
) -> Result<Vec<u8>, ProviderError> {
    read_body_source_bounded(response, limit, context).await
}

trait BodyChunkSource {
    fn content_length_hint(&self) -> Option<u64>;

    async fn next_body_chunk(
        &mut self,
    ) -> Result<Option<impl AsRef<[u8]> + Send + 'static>, ProviderError>;
}

impl BodyChunkSource for reqwest::Response {
    fn content_length_hint(&self) -> Option<u64> {
        self.content_length()
    }

    async fn next_body_chunk(
        &mut self,
    ) -> Result<Option<impl AsRef<[u8]> + Send + 'static>, ProviderError> {
        self.chunk().await.map_err(transport_error)
    }
}

async fn read_body_source_bounded<S: BodyChunkSource>(
    mut source: S,
    limit: usize,
    context: &str,
) -> Result<Vec<u8>, ProviderError> {
    if source
        .content_length_hint()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(body_too_large(context, limit));
    }
    let mut body = Vec::with_capacity(
        source
            .content_length_hint()
            .and_then(|length| usize::try_from(length).ok())
            .unwrap_or_default()
            .min(limit),
    );
    while let Some(chunk) = source.next_body_chunk().await? {
        let chunk = chunk.as_ref();
        let Some(length) = body.len().checked_add(chunk.len()) else {
            return Err(body_too_large(context, limit));
        };
        if length > limit {
            return Err(body_too_large(context, limit));
        }
        body.extend_from_slice(chunk);
    }
    Ok(body)
}

fn body_too_large(context: &str, limit: usize) -> ProviderError {
    malformed(format!("{context} body exceeded the {limit}-byte limit"))
}

#[derive(Debug, Clone, Copy)]
enum DecoderKind {
    Responses(OpenAiComputerToolKind),
    Chat(CompatibleDialect),
}

async fn stream_response(
    response: reqwest::Response,
    account: Option<CredentialAlias>,
    sender: mpsc::Sender<ProviderStreamItem>,
    chunk_idle_timeout: Duration,
    kind: DecoderKind,
) {
    stream_sse_source(response, account, sender, chunk_idle_timeout, kind).await;
}

trait SseChunkSource {
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

async fn stream_sse_source<S: SseChunkSource>(
    mut source: S,
    account: Option<CredentialAlias>,
    sender: mpsc::Sender<ProviderStreamItem>,
    chunk_idle_timeout: Duration,
    kind: DecoderKind,
) {
    let mut decoder = match kind {
        DecoderKind::Responses(computer_kind) => {
            OpenAiDecoder::Responses(ResponsesDecoder::new(account, computer_kind))
        }
        DecoderKind::Chat(dialect) => OpenAiDecoder::Chat(ChatDecoder::new(account, dialect)),
    };
    loop {
        let chunk = match tokio::time::timeout(chunk_idle_timeout, source.next_chunk()).await {
            Ok(Ok(Some(chunk))) => chunk,
            Ok(Ok(None)) => {
                let items = decoder.finish();
                let _ = send_items(&sender, items).await;
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

enum OpenAiDecoder {
    Responses(ResponsesDecoder),
    Chat(ChatDecoder),
}

impl OpenAiDecoder {
    fn push(&mut self, bytes: &[u8]) -> Vec<ProviderStreamItem> {
        match self {
            Self::Responses(decoder) => decoder.push(bytes),
            Self::Chat(decoder) => decoder.push(bytes),
        }
    }

    fn finish(&mut self) -> Vec<ProviderStreamItem> {
        match self {
            Self::Responses(decoder) => decoder.finish(),
            Self::Chat(decoder) => decoder.finish(),
        }
    }

    fn is_terminal(&self) -> bool {
        match self {
            Self::Responses(decoder) => decoder.terminal,
            Self::Chat(decoder) => decoder.terminal,
        }
    }
}

#[derive(Debug)]
struct SseFrame {
    event: Option<String>,
    data: String,
}

#[derive(Debug, Default)]
struct SseFramer {
    utf8: Utf8Assembler,
    line_buffer: String,
    event_name: Option<String>,
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
            return Err(malformed("OpenAI SSE stream ended inside a UTF-8 scalar"));
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
        match field {
            "event" => self.event_name = Some(value.to_owned()),
            "data" => self.data_lines.push(value.to_owned()),
            "id" | "retry" => {}
            _ => {}
        }
        None
    }

    fn dispatch_frame(&mut self) -> Option<SseFrame> {
        if self.data_lines.is_empty() {
            self.event_name = None;
            return None;
        }
        let frame = SseFrame {
            event: self.event_name.take(),
            data: self.data_lines.join("\n"),
        };
        self.data_lines.clear();
        Some(frame)
    }
}

#[derive(Debug)]
struct ResponsesDecoder {
    framer: SseFramer,
    account: Option<CredentialAlias>,
    computer_kind: OpenAiComputerToolKind,
    open_calls: BTreeMap<usize, ResponseFunctionCall>,
    call_items: HashMap<String, usize>,
    pending_tool_events: Vec<StreamEvent>,
    saw_tool: bool,
    saw_refusal: bool,
    terminal: bool,
}

#[derive(Debug)]
struct ResponseFunctionCall {
    call_id: String,
    ended: bool,
}

impl ResponsesDecoder {
    fn new(account: Option<CredentialAlias>, computer_kind: OpenAiComputerToolKind) -> Self {
        Self {
            framer: SseFramer::default(),
            account,
            computer_kind,
            open_calls: BTreeMap::new(),
            call_items: HashMap::new(),
            pending_tool_events: Vec::new(),
            saw_tool: false,
            saw_refusal: false,
            terminal: false,
        }
    }

    fn push(&mut self, bytes: &[u8]) -> Vec<ProviderStreamItem> {
        if self.terminal {
            return Vec::new();
        }
        let frames = match self.framer.push(bytes) {
            Ok(frames) => frames,
            Err(error) => return self.fail(error),
        };
        self.accept_frames(frames)
    }

    fn finish(&mut self) -> Vec<ProviderStreamItem> {
        if self.terminal {
            return Vec::new();
        }
        let frames = match self.framer.finish() {
            Ok(frames) => frames,
            Err(error) => return self.fail(error),
        };
        let items = self.accept_frames(frames);
        if self.terminal || items.iter().any(Result::is_err) {
            items
        } else {
            let mut items = items;
            items.push(Err(stream_interrupted(
                "OpenAI Responses SSE ended before a terminal response event",
            )));
            self.terminal = true;
            items
        }
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
        if frame.data == "[DONE]" {
            return Ok(Vec::new());
        }
        let value: serde_json::Value = match serde_json::from_str(&frame.data) {
            Ok(value) => value,
            Err(_) if matches!(frame.event.as_deref(), Some("response.failed" | "error")) => {
                return Err(openai_stream_error_prose(&frame.data));
            }
            Err(error) => {
                return Err(malformed(format!(
                    "OpenAI Responses SSE data is not valid JSON: {error}"
                )));
            }
        };
        if matches!(frame.event.as_deref(), Some("response.failed" | "error"))
            && value.get("type").is_none()
        {
            return Err(openai_stream_error(&value));
        }
        let event_type = value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| malformed("OpenAI Responses SSE event has no type"))?;
        if frame
            .event
            .as_deref()
            .is_some_and(|event| event != event_type)
        {
            return Err(malformed(format!(
                "OpenAI Responses SSE event `{}` disagrees with data type `{event_type}`",
                frame.event.as_deref().unwrap_or_default()
            )));
        }
        match event_type {
            "response.output_text.delta" => Ok(vec![StreamEvent::TextDelta {
                text: required_string(&value, "delta", event_type)?,
            }]),
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                Ok(vec![StreamEvent::ReasoningDelta {
                    text: required_string(&value, "delta", event_type)?,
                }])
            }
            "response.refusal.delta" => {
                self.saw_refusal = true;
                Ok(vec![StreamEvent::RefusalDelta {
                    text: required_string(&value, "delta", event_type)?,
                }])
            }
            "response.output_item.added" => self.output_item_added(&value),
            "response.function_call_arguments.delta" => self.function_arguments_delta(&value),
            "response.function_call_arguments.done" => self.function_arguments_done(&value),
            "response.output_item.done" => self.output_item_done(&value),
            "response.completed" => self.response_terminal(&value, false),
            "response.incomplete" => self.response_terminal(&value, true),
            "response.failed" | "error" => Err(openai_stream_error(&value)),
            _ => Ok(Vec::new()),
        }
    }

    fn output_item_added(
        &mut self,
        value: &serde_json::Value,
    ) -> Result<Vec<StreamEvent>, ProviderError> {
        let item = value
            .get("item")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| malformed("OpenAI response.output_item.added has no item"))?;
        if item.get("type").and_then(serde_json::Value::as_str) != Some("function_call") {
            return Ok(Vec::new());
        }
        let output_index = required_usize(value, "output_index", "response.output_item.added")?;
        if self.open_calls.contains_key(&output_index) {
            return Err(malformed(format!(
                "OpenAI function-call output index {output_index} started twice"
            )));
        }
        let item_id = object_string(item, "id", "response.output_item.added item")?;
        let call_id = object_string(item, "call_id", "response.output_item.added item")?;
        let name = object_string(item, "name", "response.output_item.added item")?;
        let initial_arguments = item
            .get("arguments")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        self.call_items.insert(item_id.clone(), output_index);
        self.open_calls.insert(
            output_index,
            ResponseFunctionCall {
                call_id: call_id.clone(),
                ended: false,
            },
        );
        self.saw_tool = true;
        self.pending_tool_events
            .push(StreamEvent::ToolCallStart { call_id, name });
        if !initial_arguments.is_empty() {
            let call_id = self
                .open_calls
                .get(&output_index)
                .map(|call| call.call_id.clone())
                .ok_or_else(|| malformed("OpenAI function call disappeared after start"))?;
            self.pending_tool_events
                .push(StreamEvent::ToolCallArgsDelta {
                    call_id,
                    args_fragment: initial_arguments,
                });
        }
        Ok(Vec::new())
    }

    fn function_arguments_delta(
        &mut self,
        value: &serde_json::Value,
    ) -> Result<Vec<StreamEvent>, ProviderError> {
        let index = self.call_index(value, "response.function_call_arguments.delta")?;
        let call = self.open_calls.get(&index).ok_or_else(|| {
            malformed(format!(
                "OpenAI function arguments reference unopened output index {index}"
            ))
        })?;
        if call.ended {
            return Err(malformed(format!(
                "OpenAI function arguments arrived after call `{}` ended",
                call.call_id
            )));
        }
        self.pending_tool_events
            .push(StreamEvent::ToolCallArgsDelta {
                call_id: call.call_id.clone(),
                args_fragment: required_string(
                    value,
                    "delta",
                    "response.function_call_arguments.delta",
                )?,
            });
        Ok(Vec::new())
    }

    fn function_arguments_done(
        &mut self,
        value: &serde_json::Value,
    ) -> Result<Vec<StreamEvent>, ProviderError> {
        let index = self.call_index(value, "response.function_call_arguments.done")?;
        self.end_call(index)
    }

    fn output_item_done(
        &mut self,
        value: &serde_json::Value,
    ) -> Result<Vec<StreamEvent>, ProviderError> {
        let Some(item) = value.get("item").and_then(serde_json::Value::as_object) else {
            return Ok(Vec::new());
        };
        match item.get("type").and_then(serde_json::Value::as_str) {
            Some("reasoning") => {
                return Ok(vec![StreamEvent::ProviderOpaque {
                    provider: OPENAI_PROVIDER_NAME.into(),
                    data: serde_json::Value::Object(item.clone()),
                }]);
            }
            // W-B: a HOSTED web_search_call is provider-executed — it never
            // enters the local dispatch loop. The finished item is captured
            // verbatim (the reasoning-item channel) so follow-up requests
            // echo it, and surfaced as one closed display row.
            Some("web_search_call") => {
                return Ok(hosted_web_search_call_events(item));
            }
            // W-B: url_citation annotations ride the finished message item's
            // content parts — tolerant decode into display sources.
            Some("message") => {
                let sources = url_citation_sources(item);
                if sources.is_empty() {
                    return Ok(Vec::new());
                }
                return Ok(vec![StreamEvent::WebSources { sources }]);
            }
            Some("computer_call") => {
                if self.computer_kind == OpenAiComputerToolKind::Generic {
                    return Ok(Vec::new());
                }
                self.saw_tool = true;
                return native_computer_call_events(item);
            }
            Some("function_call") => {}
            _ => return Ok(Vec::new()),
        }
        let index = required_usize(value, "output_index", "response.output_item.done")?;
        self.end_call(index)
    }

    fn call_index(&self, value: &serde_json::Value, event: &str) -> Result<usize, ProviderError> {
        if let Some(index) = value
            .get("output_index")
            .and_then(serde_json::Value::as_u64)
            .and_then(|index| usize::try_from(index).ok())
        {
            return Ok(index);
        }
        let item_id = value
            .get("item_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| malformed(format!("OpenAI {event} has no output_index or item_id")))?;
        self.call_items.get(item_id).copied().ok_or_else(|| {
            malformed(format!(
                "OpenAI {event} references unknown item `{item_id}`"
            ))
        })
    }

    fn end_call(&mut self, index: usize) -> Result<Vec<StreamEvent>, ProviderError> {
        let call = self.open_calls.get_mut(&index).ok_or_else(|| {
            malformed(format!(
                "OpenAI function-call end references unopened output index {index}"
            ))
        })?;
        if call.ended {
            return Ok(Vec::new());
        }
        call.ended = true;
        self.pending_tool_events.push(StreamEvent::ToolCallEnd {
            call_id: call.call_id.clone(),
        });
        if self.open_calls.values().all(|call| call.ended) {
            Ok(std::mem::take(&mut self.pending_tool_events))
        } else {
            Ok(Vec::new())
        }
    }

    fn response_terminal(
        &mut self,
        value: &serde_json::Value,
        incomplete: bool,
    ) -> Result<Vec<StreamEvent>, ProviderError> {
        let response = value
            .get("response")
            .ok_or_else(|| malformed("OpenAI terminal response event has no response object"))?;
        if !incomplete && self.open_calls.values().any(|call| !call.ended) {
            return Err(malformed(
                "OpenAI response.completed arrived before a function call was finalized",
            ));
        }
        if incomplete {
            self.pending_tool_events.clear();
        }
        let mut events = Vec::new();
        if let Some(usage) = response.get("usage").filter(|usage| !usage.is_null()) {
            events.push(StreamEvent::UsageUpdate(openai_usage(
                usage,
                self.account.clone(),
            )?));
        }
        let reason = if incomplete {
            match response
                .get("incomplete_details")
                .and_then(|details| details.get("reason"))
                .and_then(serde_json::Value::as_str)
            {
                Some("max_output_tokens" | "max_tokens") => FinishReason::MaxTokens,
                Some("content_filter") => FinishReason::Refusal,
                _ => FinishReason::Error,
            }
        } else if self.saw_refusal {
            FinishReason::Refusal
        } else if self.saw_tool {
            FinishReason::ToolUse
        } else {
            FinishReason::EndTurn
        };
        events.push(StreamEvent::Finish { reason });
        Ok(events)
    }

    fn fail(&mut self, error: ProviderError) -> Vec<ProviderStreamItem> {
        self.terminal = true;
        vec![Err(error)]
    }
}

fn native_computer_call_events(
    item: &serde_json::Map<String, serde_json::Value>,
) -> Result<Vec<StreamEvent>, ProviderError> {
    let call_id = object_string(item, "call_id", "OpenAI computer_call item")?;
    let native_actions = if let Some(actions) = item.get("actions") {
        actions
            .as_array()
            .ok_or_else(|| malformed("OpenAI computer_call actions must be an array"))?
            .iter()
            .collect::<Vec<_>>()
    } else {
        vec![
            item.get("action")
                .ok_or_else(|| malformed("OpenAI computer_call has no action or actions"))?,
        ]
    };
    if native_actions.is_empty() {
        return Err(malformed("OpenAI computer_call actions must not be empty"));
    }
    let mut actions = Vec::new();
    for action in native_actions {
        actions.extend(openai_computer_actions(action)?);
    }
    // Both native contracts require the updated screen as their one call
    // output. The neutral runtime executes singular actions, so make that
    // screenshot an explicit final ScreenObserve operation unless the model
    // already requested it last.
    if !matches!(actions.last(), Some(ComputerAction::Screenshot)) {
        actions.push(ComputerAction::Screenshot);
    }

    let mut events = vec![StreamEvent::ProviderOpaque {
        provider: OPENAI_PROVIDER_NAME.into(),
        data: serde_json::Value::Object(item.clone()),
    }];
    for (index, action) in actions.into_iter().enumerate() {
        let action_call_id = native_computer_action_call_id(&call_id, index);
        let arguments = serde_json::to_string(&action).map_err(|error| {
            malformed(format!(
                "OpenAI computer action could not be normalized: {error}"
            ))
        })?;
        events.push(StreamEvent::ToolCallStart {
            call_id: action_call_id.clone(),
            name: "computer".into(),
        });
        events.push(StreamEvent::ToolCallArgsDelta {
            call_id: action_call_id.clone(),
            args_fragment: arguments,
        });
        events.push(StreamEvent::ToolCallEnd {
            call_id: action_call_id,
        });
    }
    Ok(events)
}

fn native_computer_action_call_id(call_id: &str, index: usize) -> String {
    format!("{call_id}::haider-computer::{index}")
}

fn openai_computer_actions(
    action: &serde_json::Value,
) -> Result<Vec<ComputerAction>, ProviderError> {
    let object = action
        .as_object()
        .ok_or_else(|| malformed("OpenAI computer action must be an object"))?;
    let action_type = object_string(object, "type", "OpenAI computer action")?;
    let coordinate = || -> Result<(u32, u32), ProviderError> {
        Ok((
            openai_action_u32(object, "x", &action_type)?,
            openai_action_u32(object, "y", &action_type)?,
        ))
    };
    match action_type.as_str() {
        "screenshot" => Ok(vec![ComputerAction::Screenshot]),
        "click" => {
            reject_openai_action_modifiers(object, &action_type)?;
            let (x, y) = coordinate()?;
            match object
                .get("button")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("left")
            {
                "left" => Ok(vec![ComputerAction::LeftClick { x, y }]),
                "right" => Ok(vec![
                    ComputerAction::MouseMove { x, y },
                    ComputerAction::RightClick,
                ]),
                "wheel" | "middle" => Ok(vec![
                    ComputerAction::MouseMove { x, y },
                    ComputerAction::MiddleClick,
                ]),
                button => Err(malformed(format!(
                    "OpenAI computer click uses unsupported button `{button}`"
                ))),
            }
        }
        "double_click" => {
            reject_openai_action_modifiers(object, &action_type)?;
            if object
                .get("button")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|button| button != "left")
            {
                return Err(malformed(
                    "OpenAI computer double_click supports only the left button in the neutral computer vocabulary",
                ));
            }
            let (x, y) = coordinate()?;
            Ok(vec![
                ComputerAction::MouseMove { x, y },
                ComputerAction::DoubleClick,
            ])
        }
        "move" | "mouse_move" => {
            let (x, y) = coordinate()?;
            Ok(vec![ComputerAction::MouseMove { x, y }])
        }
        "drag" => {
            reject_openai_action_modifiers(object, &action_type)?;
            let path = object
                .get("path")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| malformed("OpenAI computer drag requires a path array"))?;
            if path.len() < 2 {
                return Err(malformed(
                    "OpenAI computer drag path requires at least two points",
                ));
            }
            let from = openai_action_point(&path[0], "drag path start")?;
            if path.len() == 2 {
                let to = openai_action_point(&path[1], "drag path end")?;
                return Ok(vec![ComputerAction::LeftClickDrag { from, to }]);
            }
            let mut actions = Vec::with_capacity(path.len().saturating_add(2));
            actions.push(ComputerAction::MouseMove {
                x: from.x,
                y: from.y,
            });
            actions.push(ComputerAction::LeftMouseDown);
            for (index, point) in path.iter().enumerate().skip(1) {
                let point = openai_action_point(point, &format!("drag path point {index}"))?;
                actions.push(ComputerAction::MouseMove {
                    x: point.x,
                    y: point.y,
                });
            }
            actions.push(ComputerAction::LeftMouseUp);
            Ok(actions)
        }
        "type" => Ok(vec![ComputerAction::Type {
            text: object_string(object, "text", "OpenAI computer type action")?,
        }]),
        "keypress" | "key" => {
            let keys = object
                .get("keys")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| malformed("OpenAI computer keypress requires a keys array"))?;
            let keys = keys
                .iter()
                .map(|key| {
                    key.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| malformed("OpenAI computer keypress keys must be strings"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            if keys.is_empty() {
                return Err(malformed(
                    "OpenAI computer keypress requires at least one key",
                ));
            }
            Ok(vec![ComputerAction::Key {
                keys: keys.join("+"),
            }])
        }
        "scroll" => openai_scroll_actions(object),
        "wait" => Ok(vec![ComputerAction::Wait { ms: 2_000 }]),
        other => Err(malformed(format!(
            "OpenAI computer action `{other}` is unsupported by the neutral computer vocabulary"
        ))),
    }
}

fn openai_scroll_actions(
    object: &serde_json::Map<String, serde_json::Value>,
) -> Result<Vec<ComputerAction>, ProviderError> {
    reject_openai_action_modifiers(object, "scroll")?;
    let x = openai_action_u32(object, "x", "scroll")?;
    let y = openai_action_u32(object, "y", "scroll")?;
    let scroll_x = openai_action_i64(object, "scroll_x", "scroll")?;
    let scroll_y = openai_action_i64(object, "scroll_y", "scroll")?;
    let mut actions = Vec::new();
    let mut append = |delta: i64, negative, positive| {
        if delta == 0 {
            return;
        }
        // OpenAI deltas are pixel-like. The native backends consume bounded
        // line counts; the documented harness uses one wheel step per ~100
        // delta units, rounded and never collapsed to zero.
        let magnitude = delta.unsigned_abs().saturating_add(50) / 100;
        actions.push(ComputerAction::Scroll {
            x,
            y,
            direction: if delta < 0 { negative } else { positive },
            amount: u32::try_from(magnitude.max(1)).unwrap_or(u32::MAX),
        });
    };
    append(scroll_y, ScrollDirection::Up, ScrollDirection::Down);
    append(scroll_x, ScrollDirection::Left, ScrollDirection::Right);
    if actions.is_empty() {
        return Err(malformed("OpenAI computer scroll has zero delta"));
    }
    Ok(actions)
}

fn reject_openai_action_modifiers(
    object: &serde_json::Map<String, serde_json::Value>,
    action_type: &str,
) -> Result<(), ProviderError> {
    let Some(keys) = object.get("keys") else {
        return Ok(());
    };
    let keys = keys.as_array().ok_or_else(|| {
        malformed(format!(
            "OpenAI computer action `{action_type}` has a non-array `keys` modifier"
        ))
    })?;
    if !keys.is_empty() {
        return Err(malformed(format!(
            "OpenAI computer action `{action_type}` uses modifiers that the neutral computer vocabulary cannot represent atomically"
        )));
    }
    Ok(())
}

fn openai_action_point(
    value: &serde_json::Value,
    context: &str,
) -> Result<ScreenPoint, ProviderError> {
    if let Some(coordinate) = value.as_array() {
        if coordinate.len() != 2 {
            return Err(malformed(format!(
                "OpenAI computer {context} coordinate array must contain exactly two integers"
            )));
        }
        return Ok(ScreenPoint {
            x: coordinate[0]
                .as_u64()
                .and_then(|value| u32::try_from(value).ok())
                .ok_or_else(|| {
                    malformed(format!(
                        "OpenAI computer {context} x coordinate must be a non-negative integer"
                    ))
                })?,
            y: coordinate[1]
                .as_u64()
                .and_then(|value| u32::try_from(value).ok())
                .ok_or_else(|| {
                    malformed(format!(
                        "OpenAI computer {context} y coordinate must be a non-negative integer"
                    ))
                })?,
        });
    }
    let object = value
        .as_object()
        .ok_or_else(|| malformed(format!("OpenAI computer {context} must be an object")))?;
    Ok(ScreenPoint {
        x: openai_action_u32(object, "x", context)?,
        y: openai_action_u32(object, "y", context)?,
    })
}

fn openai_action_u32(
    object: &serde_json::Map<String, serde_json::Value>,
    field: &str,
    context: &str,
) -> Result<u32, ProviderError> {
    object
        .get(field)
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| {
            malformed(format!(
                "OpenAI computer {context} requires non-negative integer `{field}`"
            ))
        })
}

fn openai_action_i64(
    object: &serde_json::Map<String, serde_json::Value>,
    field: &str,
    context: &str,
) -> Result<i64, ProviderError> {
    object
        .get(field)
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(|| {
            malformed(format!(
                "OpenAI computer {context} requires integer `{field}`"
            ))
        })
}

/// One finished hosted `web_search_call` item (W-B): captured VERBATIM for
/// the follow-up echo (the reasoning-item channel) and decoded tolerantly
/// into a closed display row — query visible when the action carries one,
/// failed status honest, absent fields never fatal.
fn hosted_web_search_call_events(
    item: &serde_json::Map<String, serde_json::Value>,
) -> Vec<StreamEvent> {
    let call_id = item
        .get("id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("web_search_call")
        .to_owned();
    let action = item
        .get("action")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let status = item
        .get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("completed");
    let preview = action
        .get("type")
        .and_then(serde_json::Value::as_str)
        .map_or_else(|| status.to_owned(), str::to_owned);
    vec![
        StreamEvent::ProviderOpaque {
            provider: OPENAI_PROVIDER_NAME.into(),
            data: serde_json::Value::Object(item.clone()),
        },
        StreamEvent::ServerToolUse {
            call_id: call_id.clone(),
            name: "web_search".into(),
            args: action,
        },
        StreamEvent::ServerToolResult {
            call_id,
            preview,
            is_error: status == "failed",
        },
    ]
}

/// Tolerantly mines `url_citation` annotations out of one finished message
/// item's content parts (W-B): sources dedup by URL; every field is optional
/// except the URL itself.
fn url_citation_sources(item: &serde_json::Map<String, serde_json::Value>) -> Vec<WebSource> {
    let mut sources: Vec<WebSource> = Vec::new();
    let Some(content) = item.get("content").and_then(serde_json::Value::as_array) else {
        return sources;
    };
    for part in content {
        let Some(annotations) = part
            .get("annotations")
            .and_then(serde_json::Value::as_array)
        else {
            continue;
        };
        for annotation in annotations {
            if annotation.get("type").and_then(serde_json::Value::as_str) != Some("url_citation") {
                continue;
            }
            let Some(url) = annotation.get("url").and_then(serde_json::Value::as_str) else {
                continue;
            };
            if sources.iter().any(|source| source.url == url) {
                continue;
            }
            sources.push(WebSource {
                url: url.to_owned(),
                title: annotation
                    .get("title")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
            });
        }
    }
    sources
}

/// The alpha/search endpoint for one subscription origin. The credential's
/// own `base_url` wins when it carries one (an imported codex credential may
/// point at a proxy); otherwise the sanctioned subscription base. Always
/// `{base}/alpha/search` — the same `{provider_base}` join codex performs.
#[must_use]
pub fn codex_alpha_search_url(base_url: Option<&str>) -> String {
    let base = base_url
        .map(str::trim)
        .filter(|base| !base.is_empty())
        .unwrap_or(OPENAI_SUBSCRIPTION_BASE_URL)
        .trim_end_matches('/');
    format!("{base}/alpha/search")
}

/// Output bound codex 0.145 sends on every alpha/search call (capture
/// 2026-08-21); the endpoint accepts it even though /responses-lite bans
/// the same field.
const ALPHA_SEARCH_MAX_OUTPUT_TOKENS: u32 = 10_000;

/// The exact SearchRequest body the daemon POSTs to
/// [`OPENAI_ALPHA_SEARCH_URL`] for the client `web_search` tool (W-B
/// decision 3, LW4 golden — re-pinned 2026-08-21 from a live codex 0.145
/// trace after the backend drifted: `commands` became an OBJECT keyed by
/// command family and the array form 400s "expected an object";
/// /Users/rizzist/haider-run/contract-capture-2026-08-21.md holds the
/// bodies). `search_query` accepts several `{q}` entries; haider sends the
/// tool's one query. `response_length` is short|long in captures (codex
/// lets the model pick); the harness fixes `long` — this tool feeds a
/// model, not a status line. `input` stays empty (codex seeds its user
/// message there as a nicety, not a requirement) and the settings mirror
/// codex: `external_web_access: false`, no `search_context_size`.
#[must_use]
pub fn codex_alpha_search_request_body(
    session_id: &str,
    model: &str,
    query: &str,
) -> serde_json::Value {
    serde_json::json!({
        "id": session_id,
        "model": model,
        "input": [],
        "commands": {
            "search_query": [{"q": query}],
            "response_length": "long",
        },
        "settings": {
            "allowed_callers": ["direct"],
            "external_web_access": false,
        },
        "max_output_tokens": ALPHA_SEARCH_MAX_OUTPUT_TOKENS,
    })
}

/// Tolerantly extracts readable text from an alpha/search response body:
/// output-item text parts first, then any top-level `output_text`/`text`
/// field, and finally the bounded raw JSON — an unofficial endpoint's shape
/// is never trusted enough to hard-fail a successful HTTP 200.
#[must_use]
pub fn codex_alpha_search_response_text(body: &[u8]) -> String {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return String::from_utf8_lossy(body).into_owned();
    };
    let mut collected = String::new();
    if let Some(output) = value.get("output").and_then(serde_json::Value::as_array) {
        for item in output {
            if let Some(text) = item.get("text").and_then(serde_json::Value::as_str) {
                collected.push_str(text);
                collected.push('\n');
            }
            if let Some(content) = item.get("content").and_then(serde_json::Value::as_array) {
                for part in content {
                    if let Some(text) = part.get("text").and_then(serde_json::Value::as_str) {
                        collected.push_str(text);
                        collected.push('\n');
                    }
                }
            }
        }
    }
    if collected.trim().is_empty()
        && let Some(text) = value
            .get("output_text")
            .or_else(|| value.get("text"))
            .and_then(serde_json::Value::as_str)
    {
        collected = text.to_owned();
    }
    if collected.trim().is_empty() {
        collected = value.to_string();
    }
    collected.trim_end().to_owned()
}

#[derive(Debug)]
struct ChatDecoder {
    framer: SseFramer,
    account: Option<CredentialAlias>,
    dialect: CompatibleDialect,
    open_calls: BTreeMap<usize, ChatFunctionCall>,
    pending_tool_events: Vec<StreamEvent>,
    finish_reason: Option<FinishReason>,
    terminal: bool,
}

#[derive(Debug)]
struct ChatFunctionCall {
    call_id: String,
    name: String,
    ended: bool,
    /// True when the SERVER omitted the tool-call id and the decoder minted
    /// the stable per-index one (G4a LK7 — vLLM/llama.cpp omit ids). Later
    /// id fields on the same index are then informational, never a
    /// consistency violation.
    synthesized_id: bool,
}

impl ChatDecoder {
    fn new(account: Option<CredentialAlias>, dialect: CompatibleDialect) -> Self {
        Self {
            framer: SseFramer::default(),
            account,
            dialect,
            open_calls: BTreeMap::new(),
            pending_tool_events: Vec::new(),
            finish_reason: None,
            terminal: false,
        }
    }

    fn push(&mut self, bytes: &[u8]) -> Vec<ProviderStreamItem> {
        if self.terminal {
            return Vec::new();
        }
        let frames = match self.framer.push(bytes) {
            Ok(frames) => frames,
            Err(error) => return self.fail(error),
        };
        self.accept_frames(frames)
    }

    fn finish(&mut self) -> Vec<ProviderStreamItem> {
        if self.terminal {
            return Vec::new();
        }
        let frames = match self.framer.finish() {
            Ok(frames) => frames,
            Err(error) => return self.fail(error),
        };
        let mut items = self.accept_frames(frames);
        if self.terminal {
            return items;
        }
        if let Some(reason) = self.finish_reason {
            items.extend(self.finish_events(reason).into_iter().map(Ok));
            self.terminal = true;
            items
        } else {
            items.push(Err(stream_interrupted(
                "OpenAI-compatible Chat SSE ended before [DONE] or finish_reason",
            )));
            self.terminal = true;
            items
        }
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
        if frame.data == "[DONE]" {
            return Ok(self.finish_events(self.finish_reason.unwrap_or(FinishReason::EndTurn)));
        }
        let value: serde_json::Value = match serde_json::from_str(&frame.data) {
            Ok(value) => value,
            Err(_) if frame.event.as_deref() == Some("error") => {
                return Err(openai_stream_error_prose(&frame.data));
            }
            Err(error) => {
                return Err(malformed(format!(
                    "OpenAI-compatible Chat SSE data is not valid JSON: {error}"
                )));
            }
        };
        if frame.event.as_deref() == Some("error") && value.get("error").is_none() {
            return Err(openai_stream_error(&value));
        }
        if value.get("error").is_some() {
            return Err(openai_stream_error(&value));
        }
        let mut events = Vec::new();
        if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
            events.push(StreamEvent::UsageUpdate(chat_usage(
                usage,
                self.account.clone(),
                self.dialect,
            )?));
        }
        let choices = value
            .get("choices")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| malformed("OpenAI-compatible Chat chunk has no choices array"))?;
        for choice in choices {
            let delta = choice.get("delta").unwrap_or(&serde_json::Value::Null);
            if let Some(text) = delta.get("content").and_then(serde_json::Value::as_str)
                && !text.is_empty()
            {
                events.push(StreamEvent::TextDelta { text: text.into() });
            }
            if let Some(text) = delta
                .get("reasoning_content")
                .or_else(|| delta.get("reasoning"))
                .and_then(serde_json::Value::as_str)
                && !text.is_empty()
            {
                events.push(StreamEvent::ReasoningDelta { text: text.into() });
            }
            if let Some(refusal) = delta.get("refusal").and_then(serde_json::Value::as_str)
                && !refusal.is_empty()
            {
                events.push(StreamEvent::RefusalDelta {
                    text: refusal.into(),
                });
                self.finish_reason = Some(FinishReason::Refusal);
            }
            if let Some(tool_calls) = delta
                .get("tool_calls")
                .and_then(serde_json::Value::as_array)
            {
                for tool_call in tool_calls {
                    events.extend(self.tool_delta(tool_call)?);
                }
            }
            if let Some(reason) = choice
                .get("finish_reason")
                .and_then(serde_json::Value::as_str)
            {
                let reason = normalize_chat_finish_reason(reason)?;
                if self
                    .finish_reason
                    .is_some_and(|existing| existing != reason)
                {
                    return Err(malformed(
                        "OpenAI-compatible Chat stream changed its finish_reason",
                    ));
                }
                self.finish_reason = Some(reason);
                if reason == FinishReason::ToolUse {
                    events.extend(self.close_calls());
                }
            }
        }
        Ok(events)
    }

    fn tool_delta(&mut self, value: &serde_json::Value) -> Result<Vec<StreamEvent>, ProviderError> {
        let index = required_usize(value, "index", "Chat tool-call delta")?;
        let id = value
            .get("id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.is_empty());
        let function = value.get("function").unwrap_or(&serde_json::Value::Null);
        let name = function
            .get("name")
            .and_then(serde_json::Value::as_str)
            .filter(|name| !name.is_empty());
        let arguments = function
            .get("arguments")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if let std::collections::btree_map::Entry::Vacant(entry) = self.open_calls.entry(index) {
            // G4a LK7 tolerance: some OSS servers (vLLM, llama.cpp) stream
            // tool-call deltas WITHOUT ids. The id only has to be stable and
            // unique within this turn, so mint one from the index instead of
            // failing the stream.
            let (call_id, synthesized_id) = match id {
                Some(id) => (id.to_owned(), false),
                None => (format!("tool-call-{index}"), true),
            };
            let name = name.ok_or_else(|| {
                malformed(format!(
                    "OpenAI-compatible tool index {index} started without a name"
                ))
            })?;
            self.pending_tool_events.push(StreamEvent::ToolCallStart {
                call_id: call_id.clone(),
                name: name.into(),
            });
            entry.insert(ChatFunctionCall {
                call_id,
                name: name.into(),
                ended: false,
                synthesized_id,
            });
        }
        let call = self.open_calls.get(&index).ok_or_else(|| {
            malformed(format!(
                "OpenAI-compatible tool index {index} disappeared after start"
            ))
        })?;
        if !call.synthesized_id && id.is_some_and(|id| id != call.call_id) {
            return Err(malformed(format!(
                "OpenAI-compatible tool index {index} changed call id"
            )));
        }
        if name.is_some_and(|name| name != call.name) {
            return Err(malformed(format!(
                "OpenAI-compatible tool index {index} changed function name"
            )));
        }
        if call.ended && !arguments.is_empty() {
            return Err(malformed(format!(
                "OpenAI-compatible arguments arrived after call `{}` ended",
                call.call_id
            )));
        }
        if !arguments.is_empty() {
            self.pending_tool_events
                .push(StreamEvent::ToolCallArgsDelta {
                    call_id: call.call_id.clone(),
                    args_fragment: arguments.into(),
                });
        }
        Ok(Vec::new())
    }

    fn close_calls(&mut self) -> Vec<StreamEvent> {
        for call in self.open_calls.values_mut() {
            if !call.ended {
                call.ended = true;
                self.pending_tool_events.push(StreamEvent::ToolCallEnd {
                    call_id: call.call_id.clone(),
                });
            }
        }
        std::mem::take(&mut self.pending_tool_events)
    }

    fn finish_events(&mut self, reason: FinishReason) -> Vec<StreamEvent> {
        // G4a LK8 tolerance: some OSS servers emit tool-call deltas yet
        // close with finish_reason "stop" instead of "tool_calls". The calls
        // are real — complete them and finish as tool use rather than
        // silently discarding a tool invocation the model asked for. Every
        // other non-tool reason (max tokens, refusal) still drops partials.
        let reason = if reason == FinishReason::EndTurn && !self.open_calls.is_empty() {
            FinishReason::ToolUse
        } else {
            reason
        };
        let mut events = if reason == FinishReason::ToolUse {
            self.close_calls()
        } else {
            self.pending_tool_events.clear();
            Vec::new()
        };
        events.push(StreamEvent::Finish { reason });
        events
    }

    fn fail(&mut self, error: ProviderError) -> Vec<ProviderStreamItem> {
        self.terminal = true;
        vec![Err(error)]
    }
}

/// Replays native Responses SSE bytes through the live incremental decoder.
#[must_use]
pub fn replay_openai_responses_sse(bytes: &[u8]) -> Vec<ProviderStreamItem> {
    replay_responses_sse(bytes, OpenAiComputerToolKind::Generic)
}

/// Replays native-computer Responses SSE bytes under the same model
/// capability negotiation used by live OpenAI requests.
#[must_use]
pub fn replay_openai_native_computer_sse(bytes: &[u8], model: &str) -> Vec<ProviderStreamItem> {
    replay_responses_sse(bytes, openai_computer_tool_kind(model, false))
}

fn replay_responses_sse(
    bytes: &[u8],
    computer_kind: OpenAiComputerToolKind,
) -> Vec<ProviderStreamItem> {
    let mut decoder = ResponsesDecoder::new(None, computer_kind);
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

/// Replays compatible Chat Completions SSE bytes through the live decoder.
#[must_use]
pub fn replay_openai_chat_sse(bytes: &[u8]) -> Vec<ProviderStreamItem> {
    replay_chat_sse(bytes, CompatibleDialect::Generic)
}

fn replay_chat_sse(bytes: &[u8], dialect: CompatibleDialect) -> Vec<ProviderStreamItem> {
    let mut decoder = ChatDecoder::new(None, dialect);
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

#[must_use]
pub fn replay_kimi_chat_sse(bytes: &[u8]) -> Vec<ProviderStreamItem> {
    replay_chat_sse(bytes, CompatibleDialect::KimiOAuth)
}

#[must_use]
pub fn replay_deepseek_chat_sse(bytes: &[u8]) -> Vec<ProviderStreamItem> {
    replay_chat_sse(bytes, CompatibleDialect::DeepSeekApi)
}

#[must_use]
pub fn replay_haider_code_chat_sse(bytes: &[u8]) -> Vec<ProviderStreamItem> {
    replay_chat_sse(bytes, CompatibleDialect::HaiderCodeApi)
}

#[must_use]
pub fn replay_xai_chat_sse(bytes: &[u8]) -> Vec<ProviderStreamItem> {
    replay_chat_sse(bytes, CompatibleDialect::XaiApi)
}

#[must_use]
pub fn replay_grok_chat_sse(bytes: &[u8]) -> Vec<ProviderStreamItem> {
    replay_chat_sse(bytes, CompatibleDialect::GrokOAuth)
}

/// Replays a fake/captured `GET /v1/models` body through the live capability
/// parser without requiring a listening socket.
pub fn replay_openai_models_response(
    model: &str,
    body: &[u8],
) -> Result<CapabilityDoc, ProviderError> {
    replay_compatible_models_response(OPENAI_COMPATIBLE_PROVIDER_NAME, model, body)
}

/// Replays DeepSeek's OpenAI-shaped `/models` response while retaining the
/// named builtin identity. The endpoint declares availability only, so the
/// resulting capability document remains deliberately conservative.
pub fn replay_deepseek_models_response(
    model: &str,
    body: &[u8],
) -> Result<CapabilityDoc, ProviderError> {
    replay_compatible_models_response(DEEPSEEK_PROVIDER_NAME, model, body)
}

/// Replays Haider Code's OpenAI-shaped `/models` response while retaining
/// the named builtin identity.
pub fn replay_haider_code_models_response(
    model: &str,
    body: &[u8],
) -> Result<CapabilityDoc, ProviderError> {
    replay_compatible_models_response(HAIDER_CODE_PROVIDER_NAME, model, body)
}

/// Replays xAI's OpenAI-shaped API model inventory.
pub fn replay_xai_models_response(
    model: &str,
    body: &[u8],
) -> Result<CapabilityDoc, ProviderError> {
    replay_compatible_models_response(XAI_PROVIDER_NAME, model, body)
}

/// Replays the Grok proxy's richer model inventory, retaining its declared
/// context window and reasoning-effort support.
pub fn replay_grok_models_response(
    model: &str,
    body: &[u8],
) -> Result<CapabilityDoc, ProviderError> {
    let value: serde_json::Value = serde_json::from_slice(body)
        .map_err(|error| malformed(format!("Grok models response is not valid JSON: {error}")))?;
    let models = crate::parse_catalog(crate::CatalogSource::GrokOAuth, &value)
        .map_err(|_| malformed("Grok models response has no usable model array"))?;
    let model = models
        .into_iter()
        .find(|entry| entry.slug == model)
        .ok_or_else(|| invalid_request("Grok does not advertise the configured model"))?;
    Ok(grok_capabilities_from_model(&model))
}

fn grok_capabilities_from_model(model: &crate::DiscoveredModel) -> CapabilityDoc {
    let reasoning = model
        .extensions
        .as_ref()
        .is_some_and(|extension| extension.supports_reasoning_effort);
    CapabilityDoc {
        provider: GROK_OAUTH_PROVIDER_NAME.into(),
        parallel_tools: FeatureResolve::Unsupported,
        streaming_tool_args: FeatureResolve::Unsupported,
        vision: FeatureResolve::Native,
        pdf_documents: FeatureResolve::ExplicitlyEmulated,
        thinking_visible: if reasoning {
            FeatureResolve::Native
        } else {
            FeatureResolve::Unsupported
        },
        context_limit: model.context_window.unwrap_or(0),
    }
}

fn replay_compatible_models_response(
    provider: &str,
    model: &str,
    body: &[u8],
) -> Result<CapabilityDoc, ProviderError> {
    let models: ModelsEnvelope = serde_json::from_slice(body).map_err(|error| {
        malformed(format!(
            "OpenAI-compatible /models response is not valid JSON: {error}"
        ))
    })?;
    if !models.data.iter().any(|entry| entry.id == model) {
        return Err(invalid_request(format!(
            "OpenAI-compatible endpoint does not advertise configured model `{model}`"
        )));
    }
    Ok(compatible_capabilities(provider))
}

/// Replays Kimi's richer `/coding/v1/models` document into an honest
/// capability document for the configured OpenAI-protocol model.
pub fn replay_kimi_models_response(
    model: &str,
    body: &[u8],
) -> Result<CapabilityDoc, ProviderError> {
    let value: serde_json::Value = serde_json::from_slice(body)
        .map_err(|error| malformed(format!("Kimi models response is not valid JSON: {error}")))?;
    let models = crate::parse_catalog(crate::CatalogSource::KimiOAuth, &value)
        .map_err(|_| malformed("Kimi models response has no usable model array"))?;
    let model = models
        .into_iter()
        .find(|entry| entry.slug == model)
        .ok_or_else(|| invalid_request("Kimi does not advertise the configured model"))?;
    kimi_capabilities_from_model(&model)
}

fn kimi_capabilities_from_model(
    model: &crate::DiscoveredModel,
) -> Result<CapabilityDoc, ProviderError> {
    let extensions = model
        .extensions
        .as_ref()
        .ok_or_else(|| malformed("Kimi model capability flags are missing"))?;
    Ok(CapabilityDoc {
        provider: KIMI_OAUTH_PROVIDER_NAME.into(),
        // Kimi declares tool support, but not these stronger exact semantics.
        parallel_tools: FeatureResolve::Unsupported,
        streaming_tool_args: FeatureResolve::Unsupported,
        vision: if extensions.supports_vision {
            FeatureResolve::Native
        } else {
            FeatureResolve::Unsupported
        },
        pdf_documents: FeatureResolve::ExplicitlyEmulated,
        thinking_visible: if extensions.supports_reasoning && extensions.supports_thinking_type {
            FeatureResolve::Native
        } else {
            FeatureResolve::Unsupported
        },
        context_limit: model.context_window.unwrap_or(0),
    })
}

/// Replays a captured non-success OpenAI-shaped HTTP response.
#[must_use]
pub fn replay_openai_http_error(
    status: u16,
    retry_after: Option<&str>,
    body: &[u8],
) -> ProviderError {
    let parsed = serde_json::from_slice::<OpenAiErrorEnvelope>(body).ok();
    let provider_detail = openai_http_error_detail(body);
    let error_type = parsed
        .as_ref()
        .and_then(|envelope| envelope.error.kind.as_deref());
    let error_code = parsed
        .as_ref()
        .and_then(|envelope| envelope.error.code.as_deref());
    let context_exceeded = matches!(
        error_code.or(error_type),
        Some(
            "context_length_exceeded"
                | "context_window_exceeded"
                | "model_context_window_exceeded"
                | "prompt_too_long"
                | "input_too_large"
        )
    );
    let quota_exhausted = matches!(
        error_code.or(error_type),
        Some("insufficient_quota" | "billing_hard_limit_reached" | "credit_balance_too_low")
    );
    let kind = match status {
        _ if quota_exhausted => ProviderErrorKind::QuotaExhausted,
        401 => ProviderErrorKind::Authentication,
        403 => ProviderErrorKind::PermissionDenied,
        429 => ProviderErrorKind::RateLimited,
        503 => ProviderErrorKind::Overloaded,
        408 | 500..=599 => ProviderErrorKind::Transport,
        _ if context_exceeded => ProviderErrorKind::ContextExceeded,
        _ => match error_code.or(error_type) {
            Some("invalid_api_key" | "authentication_error") => ProviderErrorKind::Authentication,
            Some("permission_denied") => ProviderErrorKind::PermissionDenied,
            Some("rate_limit_exceeded" | "rate_limit_error") => ProviderErrorKind::RateLimited,
            Some("server_error" | "timeout") => ProviderErrorKind::Transport,
            _ if provider_detail
                .as_deref()
                .is_some_and(openai_error_message_is_authentication) =>
            {
                ProviderErrorKind::Authentication
            }
            _ if provider_detail
                .as_deref()
                .is_some_and(openai_error_message_is_overload) =>
            {
                ProviderErrorKind::Overloaded
            }
            _ => ProviderErrorKind::InvalidRequest,
        },
    };
    let retry_after_ms = matches!(
        kind,
        ProviderErrorKind::RateLimited
            | ProviderErrorKind::Overloaded
            | ProviderErrorKind::Transport
    )
    .then(|| crate::parse_retry_after_ms(retry_after))
    .flatten();
    let message = if kind == ProviderErrorKind::QuotaExhausted {
        "provider quota/credit exhausted — retrying will not help; check billing or switch account"
            .to_owned()
    } else {
        format!("OpenAI HTTP {status} returned {}", provider_kind_name(kind))
    };
    let error = match error_code.or(error_type) {
        Some("account_deleted" | "account_not_found") => ProviderError::new_with_presentation(
            kind,
            message,
            crate::account_deleted_presentation(),
        ),
        Some("account_deactivated" | "account_revoked" | "organization_deactivated") => {
            ProviderError::new_with_presentation(
                kind,
                message,
                crate::account_revoked_presentation(),
            )
        }
        _ => ProviderError::new(kind, message),
    };
    let error = match provider_detail.as_deref() {
        Some(detail) => error.with_provider_detail(detail),
        None => error,
    };
    error
        .with_retry_after_ms(retry_after_ms)
        .with_http_metadata(status, None)
}

#[derive(Debug, Deserialize)]
struct OpenAiErrorEnvelope {
    error: OpenAiApiError,
}

#[derive(Debug, Deserialize)]
struct OpenAiApiError {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    code: Option<String>,
}

fn openai_http_error_detail(body: &[u8]) -> Option<String> {
    match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(value) => openai_error_message(&value).and_then(sanitize_openai_error_detail),
        Err(_) => std::str::from_utf8(body)
            .ok()
            .and_then(sanitize_openai_error_detail),
    }
}

fn openai_error_message(value: &serde_json::Value) -> Option<&str> {
    value
        .pointer("/error/message")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            value
                .pointer("/response/error/message")
                .and_then(serde_json::Value::as_str)
        })
        .or_else(|| value.get("message").and_then(serde_json::Value::as_str))
        .or_else(|| value.get("detail").and_then(serde_json::Value::as_str))
        .or_else(|| value.get("error").and_then(serde_json::Value::as_str))
        .or_else(|| {
            value
                .pointer("/response/error")
                .and_then(serde_json::Value::as_str)
        })
        .or_else(|| value.as_str())
}

fn sanitize_openai_error_detail(detail: &str) -> Option<String> {
    let mut words = Vec::new();
    let mut redact_next = false;
    for word in detail.split_whitespace() {
        let normalized = word
            .trim_matches(|character: char| {
                !character.is_ascii_alphanumeric() && character != '_' && character != '-'
            })
            .to_ascii_lowercase();
        if redact_next {
            words.push("[REDACTED]".to_owned());
            // `Authorization: Bearer <opaque>` is a common three-token
            // spelling. Consuming `Bearer` must not expose the token after it.
            redact_next = normalized == "bearer";
            continue;
        }
        if looks_like_provider_secret(&normalized) || has_inline_provider_secret(word) {
            words.push("[REDACTED]".to_owned());
            continue;
        }
        words.push(word.to_owned());
        redact_next = matches!(
            normalized.as_str(),
            "bearer" | "authorization" | "api_key" | "access_token" | "refresh_token"
        );
    }
    let detail = words.join(" ");
    (!detail.is_empty()).then_some(detail)
}

fn has_inline_provider_secret(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    ["authorization", "api_key", "access_token", "refresh_token"]
        .iter()
        .any(|label| {
            value.strip_prefix(label).is_some_and(|suffix| {
                suffix
                    .strip_prefix('=')
                    .or_else(|| suffix.strip_prefix(':'))
                    .is_some_and(|secret| !secret.is_empty())
            })
        })
}

fn looks_like_provider_secret(value: &str) -> bool {
    (value.starts_with("sk-") && value.len() >= 12)
        || (value.starts_with("sess-") && value.len() >= 16)
        || (value.starts_with("eyj")
            && value.len() >= 24
            && value.bytes().filter(|byte| *byte == b'.').count() >= 2)
}

fn openai_error_message_is_overload(message: &str) -> bool {
    message.to_ascii_lowercase().contains("overloaded")
}

fn openai_error_message_is_authentication(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("authentication token has been invalidated")
        || message.contains("authentication token is invalid")
        || message.contains("authentication token has expired")
        || message.contains("please sign in again")
        || message.contains("please log in again")
}

fn openai_stream_error(value: &serde_json::Value) -> ProviderError {
    let error = value
        .get("error")
        .or_else(|| {
            value
                .get("response")
                .and_then(|response| response.get("error"))
        })
        .unwrap_or(value);
    let kind = error
        .get("code")
        .and_then(serde_json::Value::as_str)
        .or_else(|| error.get("type").and_then(serde_json::Value::as_str));
    let provider_detail = openai_error_message(error).and_then(sanitize_openai_error_detail);
    let provider_kind = match kind {
        Some("invalid_api_key" | "authentication_error") => ProviderErrorKind::Authentication,
        Some("permission_denied") => ProviderErrorKind::PermissionDenied,
        Some("insufficient_quota" | "billing_hard_limit_reached" | "credit_balance_too_low") => {
            ProviderErrorKind::QuotaExhausted
        }
        Some("rate_limit_exceeded" | "rate_limit_error") => ProviderErrorKind::RateLimited,
        Some("overloaded_error") => ProviderErrorKind::Overloaded,
        Some(
            "context_length_exceeded"
            | "context_window_exceeded"
            | "model_context_window_exceeded"
            | "prompt_too_long"
            | "input_too_large",
        ) => ProviderErrorKind::ContextExceeded,
        Some("server_error" | "timeout") => ProviderErrorKind::Transport,
        _ if provider_detail
            .as_deref()
            .is_some_and(openai_error_message_is_authentication) =>
        {
            ProviderErrorKind::Authentication
        }
        // The codex backend returns overload under codes the arms above
        // don't know — the prose is the only stable marker. Misclassifying
        // it as InvalidRequest made a RETRYABLE condition error whole runs
        // (nine journaled failures before daemon.log named the cause).
        _ if provider_detail
            .as_deref()
            .is_some_and(openai_error_message_is_overload) =>
        {
            ProviderErrorKind::Overloaded
        }
        _ => ProviderErrorKind::InvalidRequest,
    };
    let provider_error = ProviderError::new(
        provider_kind,
        format!(
            "OpenAI stream returned {}",
            provider_kind_name(provider_kind)
        ),
    );
    match provider_detail.as_deref() {
        Some(detail) => provider_error.with_provider_detail(detail),
        None => provider_error,
    }
}

fn openai_stream_error_prose(detail: &str) -> ProviderError {
    let provider_detail = sanitize_openai_error_detail(detail);
    let provider_kind = if provider_detail
        .as_deref()
        .is_some_and(openai_error_message_is_authentication)
    {
        ProviderErrorKind::Authentication
    } else if provider_detail
        .as_deref()
        .is_some_and(openai_error_message_is_overload)
    {
        ProviderErrorKind::Overloaded
    } else {
        ProviderErrorKind::InvalidRequest
    };
    let provider_error = ProviderError::new(
        provider_kind,
        format!(
            "OpenAI stream returned {}",
            provider_kind_name(provider_kind)
        ),
    );
    match provider_detail.as_deref() {
        Some(detail) => provider_error.with_provider_detail(detail),
        None => provider_error,
    }
}

fn responses_request_json(
    request: &TurnRequest,
    codex_responses_lite: bool,
    effort: Option<&str>,
    hosted_web_search: bool,
) -> Result<serde_json::Value, ProviderError> {
    let cacheable_history_end = request.cache_metadata.as_ref().map_or(
        request.messages.len(),
        crate::PromptCacheMetadata::cacheable_history_end,
    );
    responses_request_json_with_boundary(
        request,
        codex_responses_lite,
        effort,
        hosted_web_search,
        cacheable_history_end,
    )
    .map(|(payload, _, _)| payload)
}

struct ResponsesNeutralRender {
    payload: serde_json::Value,
    stable_wire_end: usize,
    previous_wire_end: Option<usize>,
    message_wire_ends: Vec<usize>,
}

fn responses_request_json_with_boundary(
    request: &TurnRequest,
    codex_responses_lite: bool,
    effort: Option<&str>,
    hosted_web_search: bool,
    stable_history_end: usize,
) -> Result<(serde_json::Value, usize, Option<usize>), ProviderError> {
    let mut rendered = responses_request_json_neutral_with_boundary(
        request,
        &request.tools,
        codex_responses_lite,
        effort,
        hosted_web_search,
        stable_history_end,
    )?;
    apply_openai_cache_controls(
        request,
        codex_responses_lite,
        &mut rendered.payload,
        &rendered.message_wire_ends,
        stable_history_end,
        !request.tools.is_empty(),
        None,
    );
    Ok((
        rendered.payload,
        rendered.stable_wire_end,
        rendered.previous_wire_end,
    ))
}

fn responses_request_json_neutral_with_boundary(
    request: &TurnRequest,
    tools: &[crate::ToolDefinition],
    codex_responses_lite: bool,
    effort: Option<&str>,
    hosted_web_search: bool,
    stable_history_end: usize,
) -> Result<ResponsesNeutralRender, ProviderError> {
    let computer_kind = openai_computer_tool_kind(&request.model, codex_responses_lite);
    let computer_display = latest_computer_display_dimensions(request).unwrap_or((
        OPENAI_COMPUTER_BOOTSTRAP_WIDTH,
        OPENAI_COMPUTER_BOOTSTRAP_HEIGHT,
    ));
    let attachments = attachment_index(request)?;
    let native_computer_results = native_computer_result_index(request)?;
    let computer_result_call_ids = request
        .messages
        .iter()
        .flat_map(|message| &message.blocks)
        .filter_map(|block| match block {
            Block::ToolResult { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect::<HashSet<_>>();
    let mut input = Vec::new();
    // Current codex sends subscription instructions as the first developer
    // input item and leaves the top-level `instructions` parameter null. Keep
    // the API-key Responses shape unchanged.
    if codex_responses_lite && let Some(instructions) = &request.system_prompt {
        let instruction_block = serde_json::json!({"type": "input_text", "text": instructions});
        input.push(serde_json::json!({
            "type": "message",
            "role": "developer",
            "content": [instruction_block],
        }));
    }
    let stable_history_end = stable_history_end.min(request.messages.len());
    let mut stable_wire_end = (stable_history_end == 0).then_some(input.len());
    let previous_history_end = request
        .cache_metadata
        .as_ref()
        .and_then(|metadata| metadata.previous_stable_history_end)
        .filter(|previous| *previous <= request.messages.len());
    let mut previous_wire_end = (previous_history_end == Some(0)).then_some(input.len());
    let mut message_wire_ends = Vec::with_capacity(request.messages.len());
    for (message_index, message) in request.messages.iter().enumerate() {
        let mut content = Vec::new();
        for block in &message.blocks {
            match block {
                Block::Text { text } if message.role != MessageRole::Tool => {
                    // The Responses API types message content BY ROLE:
                    // assistant history must be `output_text` — replaying
                    // it as `input_text` is a 400 ("Supported values are:
                    // 'output_text' and 'refusal'", confirmed live
                    // 2026-07-31), which poisoned EVERY session after its
                    // first assistant reply (W5g-6).
                    let content_type = if message.role == MessageRole::Assistant {
                        "output_text"
                    } else {
                        "input_text"
                    };
                    content.push(serde_json::json!({"type": content_type, "text": text}));
                }
                Block::Text { .. } => {
                    return Err(invalid_request(
                        "OpenAI tool messages cannot contain plain text blocks",
                    ));
                }
                Block::Reasoning { .. } => {
                    return Err(invalid_request(
                        "normalized reasoning summaries cannot be replayed as OpenAI reasoning items",
                    ));
                }
                Block::ToolCall { call_id, .. }
                    if message.role == MessageRole::Assistant
                        && native_computer_results.contains_key(call_id) =>
                {
                    // The provider-native `computer_call` immediately before
                    // this normalized block is the assistant replay item.
                    // The normalized singular calls exist only for local
                    // dispatch and must not be echoed as function calls.
                }
                Block::ToolCall {
                    call_id,
                    name,
                    args,
                } if message.role == MessageRole::Assistant => {
                    flush_response_message(&mut input, message.role, &mut content);
                    let arguments = serde_json::to_string(args).map_err(|error| {
                        invalid_request(format!(
                            "OpenAI tool arguments could not be encoded: {error}"
                        ))
                    })?;
                    input.push(serde_json::json!({
                        "type": "function_call",
                        "call_id": call_id,
                        "name": name,
                        "arguments": arguments,
                    }));
                }
                Block::ToolCall { .. } => {
                    return Err(invalid_request(
                        "OpenAI function_call items are only valid in assistant messages",
                    ));
                }
                Block::ToolResult {
                    call_id,
                    preview,
                    images,
                    ..
                } if matches!(message.role, MessageRole::User | MessageRole::Tool)
                    && native_computer_results.contains_key(call_id) =>
                {
                    flush_response_message(&mut input, message.role, &mut content);
                    let target = native_computer_results.get(call_id).ok_or_else(|| {
                        internal("OpenAI native computer result index changed during shaping")
                    })?;
                    if let Some(status) = failed_computer_result_status(preview) {
                        // A GA computer_call may expand to several normalized
                        // actions followed by the required screenshot. Never
                        // hide an earlier denial/failure behind that final
                        // image or acknowledge preview safety checks for an
                        // action Haider did not complete.
                        return Err(invalid_request(format!(
                            "OpenAI native computer call `{}` did not complete: {status}",
                            target.provider_call_id
                        )));
                    }
                    if target.final_action {
                        for index in 0..target.action_count {
                            let expected =
                                native_computer_action_call_id(&target.provider_call_id, index);
                            if !computer_result_call_ids.contains(expected.as_str()) {
                                return Err(invalid_request(format!(
                                    "OpenAI native computer call `{}` is missing result {} of {}",
                                    target.provider_call_id,
                                    index + 1,
                                    target.action_count,
                                )));
                            }
                        }
                        let image = images.last().ok_or_else(|| {
                            invalid_request(format!(
                                "OpenAI native computer call `{}` completed without its required updated screenshot",
                                target.provider_call_id
                            ))
                        })?;
                        if !crate::tool_image_media_type_supported(&image.media_type) {
                            return Err(invalid_request(format!(
                                "tool image {} has unsupported media type",
                                image.artifact
                            )));
                        }
                        let data = resolved_attachment(&attachments, image.artifact.as_str())?;
                        let screenshot = match target.kind {
                            OpenAiComputerToolKind::Ga => serde_json::json!({
                                "type": "computer_screenshot",
                                "image_url": format!("data:{};base64,{data}", image.media_type),
                                "detail": "original",
                            }),
                            OpenAiComputerToolKind::Preview => serde_json::json!({
                                "type": "computer_screenshot",
                                "image_url": format!("data:{};base64,{data}", image.media_type),
                            }),
                            OpenAiComputerToolKind::Generic => {
                                return Err(internal(
                                    "generic computer result entered the native result shaper",
                                ));
                            }
                        };
                        let mut output = serde_json::json!({
                            "type": "computer_call_output",
                            "call_id": target.provider_call_id,
                            "output": screenshot,
                        });
                        if target.kind == OpenAiComputerToolKind::Preview
                            && !target.pending_safety_checks.is_empty()
                        {
                            output
                                .as_object_mut()
                                .ok_or_else(|| {
                                    internal("OpenAI computer_call_output was not an object")
                                })?
                                .insert(
                                    "acknowledged_safety_checks".into(),
                                    serde_json::Value::Array(target.pending_safety_checks.clone()),
                                );
                        }
                        input.push(output);
                    }
                }
                Block::ToolResult {
                    call_id,
                    preview,
                    images,
                    ..
                } if matches!(message.role, MessageRole::User | MessageRole::Tool) => {
                    flush_response_message(&mut input, message.role, &mut content);
                    input.push(serde_json::json!({
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": preview,
                    }));
                    if !images.is_empty() {
                        let image_content = images
                            .iter()
                            .map(|image| {
                                if !crate::tool_image_media_type_supported(&image.media_type) {
                                    return Err(invalid_request(format!(
                                        "tool image {} has unsupported media type",
                                        image.artifact
                                    )));
                                }
                                let data =
                                    resolved_attachment(&attachments, image.artifact.as_str())?;
                                Ok(serde_json::json!({
                                    "type": "input_image",
                                    "image_url": format!(
                                        "data:{};base64,{data}",
                                        image.media_type
                                    ),
                                    "detail": "auto",
                                }))
                            })
                            .collect::<Result<Vec<_>, ProviderError>>()?;
                        input.push(serde_json::json!({
                            "type": "message",
                            "role": "user",
                            "content": image_content,
                        }));
                    }
                }
                Block::ToolResult { .. } => {
                    return Err(invalid_request(
                        "OpenAI function_call_output items are only valid in user/tool messages",
                    ));
                }
                Block::Attachment(AttachmentBlock::Image { artifact, mime, .. })
                    if message.role == MessageRole::User =>
                {
                    let data = resolved_attachment(&attachments, artifact.as_str())?;
                    content.push(serde_json::json!({
                        "type": "input_image",
                        "image_url": format!("data:{mime};base64,{data}"),
                        "detail": "auto",
                    }));
                }
                Block::Attachment(AttachmentBlock::Image { .. }) => {
                    return Err(invalid_request(
                        "OpenAI image inputs are only valid in user messages",
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
                    if provider == OPENAI_PROVIDER_NAME && data.is_object() =>
                {
                    flush_response_message(&mut input, message.role, &mut content);
                    input.push(data.clone());
                }
                Block::ProviderOpaque { provider, .. } if provider == OPENAI_PROVIDER_NAME => {
                    return Err(invalid_request(
                        "OpenAI provider-opaque input item must be a JSON object",
                    ));
                }
                Block::ProviderOpaque { provider, .. } => {
                    return Err(invalid_request(format!(
                        "provider-opaque block for `{provider}` cannot be sent to OpenAI"
                    )));
                }
            }
        }
        flush_response_message(&mut input, message.role, &mut content);
        message_wire_ends.push(input.len());
        if message_index.saturating_add(1) == stable_history_end {
            stable_wire_end = Some(input.len());
        }
        if previous_history_end == Some(message_index.saturating_add(1)) {
            previous_wire_end = Some(input.len());
        }
    }
    let mut tools = tools
        .iter()
        .map(|tool| {
            if tool.name == "computer" {
                match computer_kind {
                    OpenAiComputerToolKind::Preview => {
                        return serde_json::json!({
                            "type": "computer_use_preview",
                            "display_width": computer_display.0,
                            "display_height": computer_display.1,
                            "environment": openai_computer_environment(),
                        });
                    }
                    OpenAiComputerToolKind::Ga => {
                        return serde_json::json!({"type": "computer"});
                    }
                    OpenAiComputerToolKind::Generic => {}
                }
            }
            serde_json::json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.input_schema,
                "strict": false,
            })
        })
        .collect::<Vec<_>>();
    // W-B (LW4): the hosted web_search tool joins the API-KEY Responses
    // request only. The responses-lite surface REJECTS every hosted tool
    // (codex spec_plan returns empty hosted specs on lite), so the lite
    // exclusion is STRUCTURAL here — the flag cannot reach a lite body.
    if hosted_web_search && !codex_responses_lite {
        tools.push(serde_json::json!({
            "type": "web_search",
            "search_context_size": "medium",
        }));
    }
    // The codex responses-lite endpoint (subscription OAuth) enforces its
    // own contract, confirmed against the live endpoint 2026-07-30:
    //   - `max_output_tokens` is REJECTED as unsupported;
    //   - `parallel_tool_calls` MUST be false;
    //   - `reasoning.context` MUST be `all_turns` (even for non-reasoning
    //     models).
    // The API-key Responses path keeps its original shape.
    let stable_wire_end = stable_wire_end.unwrap_or(input.len());
    let mut payload = serde_json::json!({
        "model": request.model,
        "input": input,
        "stream": true,
        "store": false,
    });
    let object = payload
        .as_object_mut()
        .ok_or_else(|| internal("OpenAI Responses request payload was not a JSON object"))?;
    if codex_responses_lite {
        object.insert("parallel_tool_calls".into(), serde_json::Value::Bool(false));
    } else {
        object.insert(
            "max_output_tokens".into(),
            serde_json::json!(request.max_tokens),
        );
    }
    if computer_kind == OpenAiComputerToolKind::Preview {
        object.insert("truncation".into(), serde_json::json!("auto"));
    }
    if !codex_responses_lite && let Some(instructions) = &request.system_prompt {
        object.insert(
            "instructions".into(),
            serde_json::Value::String(instructions.clone()),
        );
    }
    if !tools.is_empty() {
        object.insert("tools".into(), serde_json::Value::Array(tools));
    }
    // Reasoning object: `summary: auto` + encrypted-content include for
    // reasoning models; lite ADDS the required `context: all_turns` and
    // ensures the object exists even for a non-reasoning model.
    let reasoning_model = model_has_reasoning(&request.model);
    if reasoning_model || codex_responses_lite {
        let mut reasoning = serde_json::Map::new();
        if reasoning_model {
            reasoning.insert("summary".into(), serde_json::json!("auto"));
        }
        if codex_responses_lite {
            reasoning.insert("context".into(), serde_json::json!("all_turns"));
        }
        // G3: the session's effort MERGES into this one reasoning object for
        // reasoning models — `summary` and the lite-required `context` are
        // never dropped, and lite still never carries `max_output_tokens`.
        if reasoning_model && let Some(effort) = effort {
            reasoning.insert("effort".into(), serde_json::json!(effort));
        }
        object.insert("reasoning".into(), serde_json::Value::Object(reasoning));
        if reasoning_model {
            object.insert(
                "include".into(),
                serde_json::json!(["reasoning.encrypted_content"]),
            );
        }
    }
    Ok(ResponsesNeutralRender {
        payload,
        stable_wire_end,
        previous_wire_end,
        message_wire_ends,
    })
}

#[derive(Debug, Clone)]
struct NativeComputerResultTarget {
    kind: OpenAiComputerToolKind,
    provider_call_id: String,
    final_action: bool,
    action_count: usize,
    pending_safety_checks: Vec<serde_json::Value>,
}

fn native_computer_result_index(
    request: &TurnRequest,
) -> Result<HashMap<String, NativeComputerResultTarget>, ProviderError> {
    let mut results = HashMap::new();
    for item in request.messages.iter().flat_map(|message| &message.blocks) {
        let Block::ProviderOpaque { provider, data } = item else {
            continue;
        };
        if provider != OPENAI_PROVIDER_NAME
            || data.get("type").and_then(serde_json::Value::as_str) != Some("computer_call")
        {
            continue;
        }
        let object = data
            .as_object()
            .ok_or_else(|| invalid_request("OpenAI computer_call replay item must be an object"))?;
        let provider_call_id = object_string(object, "call_id", "OpenAI computer_call replay")?;
        let (kind, native_actions) = if let Some(actions) = object.get("actions") {
            (
                OpenAiComputerToolKind::Ga,
                actions
                    .as_array()
                    .ok_or_else(|| {
                        invalid_request("OpenAI computer_call replay actions must be an array")
                    })?
                    .iter()
                    .collect::<Vec<_>>(),
            )
        } else {
            (
                OpenAiComputerToolKind::Preview,
                vec![object.get("action").ok_or_else(|| {
                    invalid_request("OpenAI computer_call replay has no action or actions")
                })?],
            )
        };
        let mut actions = Vec::new();
        for action in native_actions {
            actions.extend(openai_computer_actions(action)?);
        }
        if !matches!(actions.last(), Some(ComputerAction::Screenshot)) {
            actions.push(ComputerAction::Screenshot);
        }
        if actions.is_empty() {
            return Err(invalid_request(
                "OpenAI computer_call replay actions must not be empty",
            ));
        }
        let pending_safety_checks = object
            .get("pending_safety_checks")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        let last = actions.len().saturating_sub(1);
        for index in 0..actions.len() {
            results.insert(
                native_computer_action_call_id(&provider_call_id, index),
                NativeComputerResultTarget {
                    kind,
                    provider_call_id: provider_call_id.clone(),
                    final_action: index == last,
                    action_count: actions.len(),
                    pending_safety_checks: pending_safety_checks.clone(),
                },
            );
        }
    }
    Ok(results)
}

fn openai_computer_tool_kind(model: &str, codex_responses_lite: bool) -> OpenAiComputerToolKind {
    // The subscription endpoint's responses-lite contract rejects hosted
    // tools. Keep its generic function declaration byte-for-byte unchanged
    // even when the underlying model also exists on the public API.
    if codex_responses_lite {
        return OpenAiComputerToolKind::Generic;
    }
    if matches!(
        model,
        "computer-use-preview" | "computer-use-preview-2025-03-11"
    ) {
        return OpenAiComputerToolKind::Preview;
    }
    if openai_ga_computer_model(model) {
        return OpenAiComputerToolKind::Ga;
    }
    OpenAiComputerToolKind::Generic
}

fn openai_computer_environment() -> &'static str {
    // The preview contract uses this to tune its interaction policy. Haider's
    // computer backend controls the host desktop, not an isolated browser.
    #[cfg(target_os = "macos")]
    {
        "mac"
    }
    #[cfg(target_os = "linux")]
    {
        "linux"
    }
    #[cfg(target_os = "windows")]
    {
        "windows"
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        "browser"
    }
}

fn failed_computer_result_status(preview: &str) -> Option<&'static str> {
    let value = serde_json::from_str::<serde_json::Value>(preview).ok()?;
    let status = value.get("status")?.as_str()?;
    match status {
        "denied" => Some("denied"),
        "rejected" => Some("rejected"),
        "cancelled" => Some("cancelled"),
        "failed" => Some("failed"),
        _ => None,
    }
}

fn openai_ga_computer_model(model: &str) -> bool {
    // Hosted-tool negotiation must fail closed. A numerically newer or
    // otherwise unknown model is not evidence that the Responses endpoint
    // accepts the GA `computer` tool. Add newly documented model families to
    // this table only after their native-tool contract is verified.
    ["gpt-5.4", "gpt-5.5", "gpt-5.6"]
        .iter()
        .any(|base| model == *base || model.starts_with(&format!("{base}-")))
}

fn latest_computer_display_dimensions(request: &TurnRequest) -> Option<(u32, u32)> {
    let computer_calls = request
        .messages
        .iter()
        .flat_map(|message| &message.blocks)
        .filter_map(|block| match block {
            Block::ToolCall { call_id, name, .. } if name == "computer" => Some(call_id.as_str()),
            _ => None,
        })
        .collect::<HashSet<_>>();
    request
        .messages
        .iter()
        .rev()
        .flat_map(|message| message.blocks.iter().rev())
        .find_map(|block| match block {
            Block::ToolResult {
                call_id, images, ..
            } if computer_calls.contains(call_id.as_str()) => images
                .last()
                .map(|image| (image.width, image.height))
                .filter(|(width, height)| *width > 0 && *height > 0),
            _ => None,
        })
}

fn openai_explicit_cache_enabled(request: &TurnRequest, codex_responses_lite: bool) -> bool {
    !codex_responses_lite
        && request.cache_metadata.as_ref().is_some_and(|metadata| {
            metadata.boundaries_valid(request.messages.len())
                && metadata.provider == OPENAI_PROVIDER_NAME
                && metadata
                    .account_scope
                    .as_deref()
                    .is_some_and(|scope| !scope.is_empty())
                && crate::cache_placement_capabilities(&metadata.provider, &request.model)
                    .marker_mode
                    == crate::CacheMarkerMode::OpenAiExplicit
        })
}

fn openai_lite_cache_breakpoint_enabled() -> bool {
    // HAIDER953 experiment, default OFF — VERDICT (2026-08-24, live): the
    // HTTPS responses-lite surface (POST chatgpt.com/backend-api/codex/
    // responses, installed signed daemon, gated request) REJECTED the field
    // with HTTP 400 (typed provider_error). Explicit breakpoints are NOT
    // available on this transport; the public-API parameter does not carry
    // over (the v0.0.947 law, now confirmed by same-surface evidence). The
    // gate stays as the documented negative result; do not enable.
    std::env::var(OPENAI_LITE_CACHE_BREAKPOINT_ENV).is_ok_and(|value| value == "1")
}

fn openai_automatic_cache_key_supported(model: &str) -> bool {
    let model_or_variant = |base: &str| model == base || model.starts_with(&format!("{base}-"));
    model_or_variant("gpt-4o")
        || model_or_variant("gpt-4.1")
        || model_or_variant("gpt-5")
        || [
            "gpt-5.1", "gpt-5.2", "gpt-5.3", "gpt-5.4", "gpt-5.5", "gpt-5.6",
        ]
        .iter()
        .any(|base| model_or_variant(base))
        || model_or_variant("o1")
        || model_or_variant("o3")
        || model_or_variant("o4")
}

/// On the subscription lite dialect the key is REQUIRED for reliable cache
/// hits: without it OpenAI's implicit prefix cache is best-effort and
/// shard-routed, and fast agentic rounds always outran its async warm-up
/// (observed 0%-cached rounds on live sessions, 2026-08-21). codex 0.145
/// sends a stable routing key. Prefix bytes still decide whether a cached
/// entry matches; the key keeps one session, plus only a fork whose exact C3
/// inherited segment is still active, on one provider route without crossing
/// an account boundary. The base is the exact provider-view header epoch, so
/// there is no second competing system/tool digest path.
fn openai_prompt_cache_key(request: &TurnRequest) -> Option<String> {
    openai_prompt_cache_key_with_header(request, None)
}

fn openai_prompt_cache_key_with_header(
    request: &TurnRequest,
    header_epoch: Option<&str>,
) -> Option<String> {
    if !openai_automatic_cache_key_supported(&request.model) {
        return None;
    }
    let metadata = request.cache_metadata.as_ref()?;
    if !metadata.boundaries_valid(request.messages.len())
        || !matches!(
            metadata.provider.as_str(),
            OPENAI_PROVIDER_NAME | OPENAI_OAUTH_PROVIDER_NAME
        )
    {
        return None;
    }
    prompt_cache_cohort_key_with_header(request, metadata, header_epoch)
}

/// Refreshes the coupled OpenAI routing fields after the provider-view header
/// has been finalized. A prepared payload must never retain a TTL without its
/// cohort key, or gain a cohort key while silently losing the explicit TTL.
fn refresh_openai_cache_routing(
    request: &TurnRequest,
    codex_responses_lite: bool,
    payload: &mut serde_json::Value,
    header_epoch: Option<&str>,
) {
    let Some(object) = payload.as_object_mut() else {
        return;
    };
    let key = header_epoch.map_or_else(
        || openai_prompt_cache_key(request),
        |header_epoch| openai_prompt_cache_key_with_header(request, Some(header_epoch)),
    );
    let Some(key) = key else {
        object.remove("prompt_cache_key");
        object.remove("prompt_cache_options");
        return;
    };
    object.insert("prompt_cache_key".into(), serde_json::Value::String(key));
    if openai_explicit_cache_enabled(request, codex_responses_lite) {
        object.insert(
            "prompt_cache_options".into(),
            serde_json::json!({"mode": "explicit", "ttl": "30m"}),
        );
    } else {
        object.remove("prompt_cache_options");
    }
}

/// Applies the thin cache-control overlay to a cache-neutral Responses DOM.
/// Normalized-message boundaries were captured during the only render, so
/// provider-opaque/signed objects are never traversed or reconstructed.
fn apply_openai_cache_controls(
    request: &TurnRequest,
    codex_responses_lite: bool,
    payload: &mut serde_json::Value,
    message_wire_ends: &[usize],
    stable_history_end: usize,
    has_tools: bool,
    header_epoch: Option<&str>,
) {
    let Some(input) = payload
        .get_mut("input")
        .and_then(serde_json::Value::as_array_mut)
    else {
        refresh_openai_cache_routing(request, codex_responses_lite, payload, header_epoch);
        return;
    };
    if codex_responses_lite
        && request.cache_metadata.is_some()
        && request.system_prompt.is_some()
        && openai_lite_cache_breakpoint_enabled()
        && let Some(object) = input
            .first_mut()
            .and_then(|item| item.get_mut("content"))
            .and_then(serde_json::Value::as_array_mut)
            .and_then(|content| content.first_mut())
            .and_then(serde_json::Value::as_object_mut)
    {
        object.insert(
            "prompt_cache_breakpoint".into(),
            serde_json::json!({"mode": "explicit"}),
        );
    }
    if openai_explicit_cache_enabled(request, codex_responses_lite)
        && let Some(metadata) = request.cache_metadata.as_ref()
    {
        let plan = crate::plan_inline_breakpoints(
            &metadata.provider,
            &request.model,
            &request.messages,
            stable_history_end,
            metadata.previous_stable_history_end,
            metadata.latest_compaction_summary_end,
            request.system_prompt.is_some(),
            has_tools,
            metadata.stable_prefix_tokens,
        );
        for boundary in plan.history_ends {
            if let Some(wire_end) = boundary
                .checked_sub(1)
                .and_then(|index| message_wire_ends.get(index))
                .copied()
            {
                let wire_end = wire_end.min(input.len());
                mark_latest_openai_cacheable_block(&mut input[..wire_end]);
            }
        }
    }
    refresh_openai_cache_routing(request, codex_responses_lite, payload, header_epoch);
}

fn openai_cache_control_observation(
    request: &TurnRequest,
    payload: &serde_json::Value,
) -> haider_protocol::provider::CacheControlObservationV1 {
    use haider_protocol::provider::{CacheControlObservationV1, CacheControlOmissionReasonV1};

    if payload.get("prompt_cache_key").is_some() {
        let ttl_ms = if payload.get("prompt_cache_options").is_some() {
            Some(30 * 60 * 1_000)
        } else {
            None
        };
        return CacheControlObservationV1::Emitted { ttl_ms };
    }
    if !openai_automatic_cache_key_supported(&request.model) {
        return CacheControlObservationV1::NotEmitted {
            reason: CacheControlOmissionReasonV1::UnsupportedModel,
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
        metadata.provider.as_str(),
        OPENAI_PROVIDER_NAME | OPENAI_OAUTH_PROVIDER_NAME
    ) {
        CacheControlOmissionReasonV1::ProviderMismatch
    } else {
        CacheControlOmissionReasonV1::AdapterUnavailable
    };
    CacheControlObservationV1::NotEmitted { reason }
}

fn compatible_cache_control_observation(
    request: &TurnRequest,
    payload: &serde_json::Value,
    dialect: CompatibleDialect,
    header_epoch: Option<&str>,
) -> haider_protocol::provider::CacheControlObservationV1 {
    use haider_protocol::provider::{CacheControlObservationV1, CacheControlOmissionReasonV1};

    match dialect {
        CompatibleDialect::DeepSeekApi => CacheControlObservationV1::NotRequired,
        CompatibleDialect::Generic | CompatibleDialect::KimiOAuth
            if payload.get("prompt_cache_key").is_some() =>
        {
            CacheControlObservationV1::Emitted { ttl_ms: None }
        }
        CompatibleDialect::Generic => {
            let Some(metadata) = request.cache_metadata.as_ref() else {
                return CacheControlObservationV1::NotEmitted {
                    reason: CacheControlOmissionReasonV1::AdapterUnavailable,
                };
            };
            let reason = if !metadata.boundaries_valid(request.messages.len()) {
                CacheControlOmissionReasonV1::InvalidBoundaries
            } else if metadata.account_scope.is_none() {
                CacheControlOmissionReasonV1::MissingAccountScope
            } else if crate::BUILTIN_PROVIDER_NAMES.contains(&metadata.provider.as_str()) {
                CacheControlOmissionReasonV1::ProviderMismatch
            } else {
                CacheControlOmissionReasonV1::AdapterUnavailable
            };
            CacheControlObservationV1::NotEmitted { reason }
        }
        CompatibleDialect::KimiOAuth => {
            let Some(metadata) = request.cache_metadata.as_ref() else {
                return CacheControlObservationV1::NotEmitted {
                    reason: CacheControlOmissionReasonV1::AdapterUnavailable,
                };
            };
            let reason = if !metadata.boundaries_valid(request.messages.len()) {
                CacheControlOmissionReasonV1::InvalidBoundaries
            } else if metadata.account_scope.is_none() {
                CacheControlOmissionReasonV1::MissingAccountScope
            } else if metadata.provider != KIMI_OAUTH_PROVIDER_NAME {
                CacheControlOmissionReasonV1::ProviderMismatch
            } else {
                CacheControlOmissionReasonV1::AdapterUnavailable
            };
            CacheControlObservationV1::NotEmitted { reason }
        }
        CompatibleDialect::XaiApi
            if xai_prompt_cache_conversation_id(request, header_epoch).is_some() =>
        {
            CacheControlObservationV1::Emitted { ttl_ms: None }
        }
        CompatibleDialect::XaiApi => {
            let Some(metadata) = request.cache_metadata.as_ref() else {
                return CacheControlObservationV1::NotEmitted {
                    reason: CacheControlOmissionReasonV1::AdapterUnavailable,
                };
            };
            let reason = if !metadata.boundaries_valid(request.messages.len()) {
                CacheControlOmissionReasonV1::InvalidBoundaries
            } else if metadata.account_scope.is_none() {
                CacheControlOmissionReasonV1::MissingAccountScope
            } else if metadata.provider != XAI_PROVIDER_NAME {
                CacheControlOmissionReasonV1::ProviderMismatch
            } else {
                CacheControlOmissionReasonV1::AdapterUnavailable
            };
            CacheControlObservationV1::NotEmitted { reason }
        }
        CompatibleDialect::HaiderCodeApi | CompatibleDialect::GrokOAuth => {
            CacheControlObservationV1::Unavailable
        }
    }
}

/// xAI Chat Completions caches matching message prefixes automatically, while
/// `x-grok-conv-id` provides the sticky route needed for reliable reuse.
fn xai_prompt_cache_conversation_id(
    request: &TurnRequest,
    header_epoch: Option<&str>,
) -> Option<String> {
    let metadata = request.cache_metadata.as_ref()?;
    if !metadata.boundaries_valid(request.messages.len()) || metadata.provider != XAI_PROVIDER_NAME
    {
        return None;
    }
    prompt_cache_cohort_key_with_header(request, metadata, header_epoch)
}

/// Custom OpenAI-compatible profiles use the same v4 routing cohort as the
/// named OpenAI-family adapters. The provider name is intentionally not a
/// constructor constant: custom aliases are daemon-owned metadata carried in
/// the request, and the resolved account scope provides the hard isolation
/// boundary (including the synthetic alias used by no-auth profiles).
fn custom_prompt_cache_key(
    request: &TurnRequest,
    metadata: &crate::PromptCacheMetadata,
    header_epoch: Option<&str>,
) -> Option<String> {
    if !metadata.boundaries_valid(request.messages.len())
        || crate::BUILTIN_PROVIDER_NAMES.contains(&metadata.provider.as_str())
    {
        return None;
    }
    prompt_cache_cohort_key_with_header(request, metadata, header_epoch)
}

/// Cache-cohort isolation law: a route exists only for a daemon-resolved
/// account and a non-empty session/cohort identity. Unrelated sessions default
/// to their own `session_scope`; only C3 forks whose exact inherited
/// provider-view segment is still active carry the fork-root route in
/// `cache_cohort`.
/// Provider/account/model, the finalized provider-view header, the full cache
/// epoch, and the output budget remain hard domain boundaries. Immutable
/// history bytes stay at the provider's prefix-match layer; a divergent fork
/// never receives the inherited cohort in the first place.
fn prompt_cache_cohort_key(
    request: &TurnRequest,
    metadata: &crate::PromptCacheMetadata,
) -> Option<String> {
    prompt_cache_cohort_key_with_header(request, metadata, None)
}

fn prompt_cache_cohort_key_with_header(
    request: &TurnRequest,
    metadata: &crate::PromptCacheMetadata,
    header_epoch: Option<&str>,
) -> Option<String> {
    if request.max_tokens == 0 {
        return None;
    }
    let account_scope = metadata
        .account_scope
        .as_deref()
        .filter(|scope| !scope.is_empty())?;
    let cache_epoch =
        (!metadata.cache_epoch.is_empty()).then_some(metadata.cache_epoch.as_str())?;
    let header_epoch = header_epoch
        .filter(|header_epoch| !header_epoch.is_empty())
        .or_else(|| {
            (!metadata.header_epoch.is_empty()).then_some(metadata.header_epoch.as_str())
        })?;
    let cohort = metadata
        .cache_cohort
        .as_deref()
        .filter(|cohort| !cohort.is_empty())
        .or_else(|| {
            (!metadata.session_scope.is_empty()).then_some(metadata.session_scope.as_str())
        })?;
    let domain = serde_json::json!({
        "schema": "haider.prompt-cache-cohort.v4",
        "provider": metadata.provider,
        "model": request.model,
        "max_tokens": request.max_tokens,
        "account_scope": account_scope,
        "header_epoch": header_epoch,
        "cache_epoch": cache_epoch,
        "cohort": cohort,
    });
    serde_json::to_vec(&domain)
        .ok()
        .map(|bytes| blake3::hash(&bytes).to_hex().to_string())
}

fn mark_latest_openai_cacheable_block(input: &mut [serde_json::Value]) {
    for item in input.iter_mut().rev() {
        let Some(content) = item
            .get_mut("content")
            .and_then(serde_json::Value::as_array_mut)
        else {
            continue;
        };
        for block in content.iter_mut().rev() {
            let supported = block
                .get("type")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|kind| matches!(kind, "input_text" | "input_image" | "input_file"));
            if supported {
                if let Some(object) = block.as_object_mut() {
                    object.insert(
                        "prompt_cache_breakpoint".into(),
                        serde_json::json!({"mode": "explicit"}),
                    );
                }
                return;
            }
        }
    }
}

fn flush_response_message(
    input: &mut Vec<serde_json::Value>,
    role: MessageRole,
    content: &mut Vec<serde_json::Value>,
) {
    if content.is_empty() {
        return;
    }
    let role = match role {
        MessageRole::User | MessageRole::Tool => "user",
        MessageRole::Assistant => "assistant",
    };
    input.push(serde_json::json!({
        "type": "message",
        "role": role,
        "content": std::mem::take(content),
    }));
}

fn chat_request_json(
    request: &TurnRequest,
    dialect: CompatibleDialect,
    kimi_thinking: Option<&KimiThinkingConfig>,
    kimi_reasoning_effort: Option<&str>,
) -> Result<serde_json::Value, ProviderError> {
    chat_request_json_with_boundary(
        request,
        &request.tools,
        dialect,
        kimi_thinking,
        kimi_reasoning_effort,
        request.messages.len(),
    )
    .map(|(payload, _, _)| payload)
}

fn chat_request_json_with_boundary(
    request: &TurnRequest,
    tools: &[crate::ToolDefinition],
    dialect: CompatibleDialect,
    kimi_thinking: Option<&KimiThinkingConfig>,
    kimi_reasoning_effort: Option<&str>,
    stable_history_end: usize,
) -> Result<(serde_json::Value, usize, Option<usize>), ProviderError> {
    let attachments = attachment_index(request)?;
    let mut messages = Vec::new();
    if let Some(system) = &request.system_prompt {
        messages.push(serde_json::json!({"role": "system", "content": system}));
    }
    let stable_history_end = stable_history_end.min(request.messages.len());
    let mut stable_wire_end = (stable_history_end == 0).then_some(messages.len());
    let previous_history_end = request
        .cache_metadata
        .as_ref()
        .and_then(|metadata| metadata.previous_stable_history_end)
        .filter(|previous| *previous <= request.messages.len());
    let mut previous_wire_end = (previous_history_end == Some(0)).then_some(messages.len());
    for (message_index, message) in request.messages.iter().enumerate() {
        match message.role {
            MessageRole::Assistant => {
                let mut text = String::new();
                let mut tool_calls = Vec::new();
                for block in &message.blocks {
                    match block {
                        Block::Text { text: delta } => text.push_str(delta),
                        Block::ToolCall {
                            call_id,
                            name,
                            args,
                        } => {
                            let arguments = serde_json::to_string(args).map_err(|error| {
                                invalid_request(format!(
                                    "OpenAI-compatible tool arguments could not be encoded: {error}"
                                ))
                            })?;
                            tool_calls.push(serde_json::json!({
                                "id": call_id,
                                "type": "function",
                                "function": {"name": name, "arguments": arguments},
                            }));
                        }
                        Block::ProviderOpaque { provider, data }
                            if provider == OPENAI_COMPATIBLE_PROVIDER_NAME && data.is_object() =>
                        {
                            messages.push(data.clone());
                        }
                        Block::Reasoning { .. } => {
                            return Err(invalid_request(
                                "normalized reasoning summaries cannot be replayed on the OpenAI-compatible wire",
                            ));
                        }
                        _ => {
                            return Err(invalid_request(
                                "OpenAI-compatible assistant messages support only text and tool calls",
                            ));
                        }
                    }
                }
                let mut wire = serde_json::json!({
                    "role": "assistant",
                    "content": (!text.is_empty()).then_some(text),
                });
                if !tool_calls.is_empty() {
                    wire.as_object_mut()
                        .ok_or_else(|| internal("Chat assistant message was not an object"))?
                        .insert("tool_calls".into(), serde_json::Value::Array(tool_calls));
                }
                messages.push(wire);
            }
            MessageRole::User => {
                let mut content = Vec::new();
                let mut results = Vec::new();
                for block in &message.blocks {
                    match block {
                        Block::Text { text } => {
                            content.push(serde_json::json!({"type": "text", "text": text}));
                        }
                        Block::Attachment(AttachmentBlock::Image { artifact, mime, .. }) => {
                            let data = resolved_attachment(&attachments, artifact.as_str())?;
                            content.push(serde_json::json!({
                                "type": "image_url",
                                "image_url": {
                                    "url": format!("data:{mime};base64,{data}"),
                                    "detail": "auto",
                                }
                            }));
                        }
                        Block::Attachment(AttachmentBlock::Pdf { artifact, .. }) => {
                            return Err(invalid_request(format!(
                                "PDF attachment `{artifact}` was not resolved by the prompt compiler"
                            )));
                        }
                        Block::ToolResult {
                            call_id,
                            preview,
                            images,
                            ..
                        } => results.push((call_id, preview, images)),
                        Block::ProviderOpaque { provider, data }
                            if provider == OPENAI_COMPATIBLE_PROVIDER_NAME && data.is_object() =>
                        {
                            messages.push(data.clone());
                        }
                        Block::Reasoning { .. } => {
                            return Err(invalid_request(
                                "normalized reasoning summaries cannot be replayed on the OpenAI-compatible wire",
                            ));
                        }
                        _ => {
                            return Err(invalid_request(
                                "OpenAI-compatible user message contains an unsupported block",
                            ));
                        }
                    }
                }
                if !content.is_empty() {
                    messages.push(serde_json::json!({"role": "user", "content": content}));
                }
                for (call_id, preview, images) in results {
                    messages.push(serde_json::json!({
                        "role": "tool",
                        "tool_call_id": call_id,
                        "content": preview,
                    }));
                    append_chat_tool_images(&mut messages, &attachments, images)?;
                }
            }
            MessageRole::Tool => {
                for block in &message.blocks {
                    match block {
                        Block::ToolResult {
                            call_id,
                            preview,
                            images,
                            ..
                        } => {
                            messages.push(serde_json::json!({
                                "role": "tool",
                                "tool_call_id": call_id,
                                "content": preview,
                            }));
                            append_chat_tool_images(&mut messages, &attachments, images)?;
                        }
                        _ => {
                            return Err(invalid_request(
                                "OpenAI-compatible tool messages require tool-result blocks",
                            ));
                        }
                    }
                }
            }
        }
        if message_index.saturating_add(1) == stable_history_end {
            stable_wire_end = Some(messages.len());
        }
        if previous_history_end == Some(message_index.saturating_add(1)) {
            previous_wire_end = Some(messages.len());
        }
    }
    let tools = tools
        .iter()
        .map(|tool| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.input_schema,
                    "strict": false,
                }
            })
        })
        .collect::<Vec<_>>();
    let stable_wire_end = stable_wire_end.unwrap_or(messages.len());
    let mut payload = serde_json::json!({
        "model": request.model,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    let object = payload
        .as_object_mut()
        .ok_or_else(|| internal("Chat request payload was not an object"))?;
    match dialect {
        CompatibleDialect::Generic => {
            object.insert("max_tokens".into(), serde_json::json!(request.max_tokens));
            if let Some(key) = request
                .cache_metadata
                .as_ref()
                .and_then(|metadata| custom_prompt_cache_key(request, metadata, None))
            {
                object.insert("prompt_cache_key".into(), serde_json::Value::String(key));
            }
        }
        CompatibleDialect::DeepSeekApi
        | CompatibleDialect::HaiderCodeApi
        | CompatibleDialect::XaiApi
        | CompatibleDialect::GrokOAuth => {
            object.insert("max_tokens".into(), serde_json::json!(request.max_tokens));
        }
        CompatibleDialect::KimiOAuth => {
            object.insert(
                "max_completion_tokens".into(),
                serde_json::json!(request.max_tokens),
            );
            if let Some(thinking) = kimi_thinking {
                // Kimi documents this as the OpenAI SDK's
                // `extra_body={"thinking": ...}` seam. SDK extra-body fields
                // are merged into the HTTP JSON, so the wire key is top-level.
                object.insert("thinking".into(), serde_json::json!(thinking));
            }
            if let Some(effort) = kimi_reasoning_effort {
                // k3-style always-thinking models take the documented
                // top-level knob; catalog gating happens at the factory.
                object.insert("reasoning_effort".into(), serde_json::json!(effort));
            }
            if let Some(key) = request.cache_metadata.as_ref().and_then(|metadata| {
                if metadata.boundaries_valid(request.messages.len())
                    && metadata.provider == KIMI_OAUTH_PROVIDER_NAME
                {
                    prompt_cache_cohort_key(request, metadata)
                } else {
                    None
                }
            }) {
                object.insert("prompt_cache_key".into(), serde_json::Value::String(key));
            }
        }
    }
    if !tools.is_empty() {
        object.insert("tools".into(), serde_json::Value::Array(tools));
    }
    Ok((payload, stable_wire_end, previous_wire_end))
}

/// OpenAI-compatible ordering law: the tool-role result is immediately
/// followed by one user-role multimodal image message for that exact result.
/// No unrelated message may be interposed between the pair.
fn append_chat_tool_images(
    messages: &mut Vec<serde_json::Value>,
    attachments: &HashMap<&str, &str>,
    images: &[haider_protocol::tool::ImageBlockRef],
) -> Result<(), ProviderError> {
    if images.is_empty() {
        return Ok(());
    }
    let content = images
        .iter()
        .map(|image| {
            if !crate::tool_image_media_type_supported(&image.media_type) {
                return Err(invalid_request(format!(
                    "tool image {} has unsupported media type",
                    image.artifact
                )));
            }
            let data = resolved_attachment(attachments, image.artifact.as_str())?;
            Ok(serde_json::json!({
                "type": "image_url",
                "image_url": {
                    "url": format!("data:{};base64,{data}", image.media_type),
                    "detail": "auto",
                }
            }))
        })
        .collect::<Result<Vec<_>, ProviderError>>()?;
    messages.push(serde_json::json!({"role": "user", "content": content}));
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
    artifact: &str,
) -> Result<&'a str, ProviderError> {
    attachments.get(artifact).copied().ok_or_else(|| {
        invalid_request(format!(
            "image attachment `{artifact}` has no resolved base64 data"
        ))
    })
}

fn native_capabilities(model: &str) -> CapabilityDoc {
    let context_limit = if model.starts_with("gpt-5.4")
        || model.starts_with("gpt-5.5")
        || model.starts_with("gpt-5.6")
        || model.starts_with("gpt-4.1")
    {
        1_000_000
    } else if model.starts_with("gpt-5") {
        400_000
    } else {
        128_000
    };
    CapabilityDoc {
        provider: OPENAI_PROVIDER_NAME.into(),
        parallel_tools: FeatureResolve::Native,
        streaming_tool_args: FeatureResolve::Native,
        vision: FeatureResolve::Native,
        pdf_documents: FeatureResolve::ExplicitlyEmulated,
        thinking_visible: if model_has_reasoning(model) {
            FeatureResolve::Native
        } else {
            FeatureResolve::Unsupported
        },
        context_limit,
    }
}

fn compatible_capabilities(provider: &str) -> CapabilityDoc {
    CapabilityDoc {
        provider: provider.into(),
        // The generic OpenAI schema and its model-list shape carry none of
        // these feature or limit facts, so do not infer them from a
        // vendor-controlled model identifier.
        parallel_tools: FeatureResolve::Unsupported,
        streaming_tool_args: FeatureResolve::Unsupported,
        vision: FeatureResolve::Unsupported,
        pdf_documents: FeatureResolve::ExplicitlyEmulated,
        thinking_visible: FeatureResolve::Unsupported,
        context_limit: 0,
    }
}

fn unavailable_compatible_capabilities(provider: &str) -> CapabilityDoc {
    CapabilityDoc {
        provider: provider.into(),
        parallel_tools: FeatureResolve::Unsupported,
        streaming_tool_args: FeatureResolve::Unsupported,
        vision: FeatureResolve::Unsupported,
        pdf_documents: FeatureResolve::ExplicitlyEmulated,
        thinking_visible: FeatureResolve::Unsupported,
        context_limit: 0,
    }
}

fn model_has_reasoning(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model.starts_with("gpt-5")
        || model.starts_with("o1")
        || model.starts_with("o3")
        || model.starts_with("o4")
        || model.contains("reasoning")
        || model.contains("deepseek-r1")
        || model.contains("gpt-oss")
}

#[derive(Debug)]
pub(crate) struct CompatibleEndpoints {
    pub(crate) base_url: String,
    chat_url: String,
    pub(crate) models_url: String,
    origin: Option<CompatibleHostnameOrigin>,
}

#[derive(Debug)]
struct CompatibleHostnameOrigin {
    host: String,
    port: u16,
    plain_http: bool,
}

pub(crate) fn compatible_endpoints(
    base_url: &str,
    policy: CompatibleOriginPolicy,
) -> Result<CompatibleEndpoints, ProviderError> {
    let base_url = base_url.trim().trim_end_matches('/');
    if base_url.is_empty() {
        return Err(invalid_request(
            "OpenAI-compatible credentials require a base_url",
        ));
    }
    let parsed = reqwest::Url::parse(base_url)
        .map_err(|error| invalid_request(format!("invalid OpenAI-compatible base_url: {error}")))?;
    if !matches!(parsed.scheme(), "http" | "https")
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(invalid_request(
            "OpenAI-compatible base_url must be an http(s) URL without credentials, query, or fragment",
        ));
    }
    validate_compatible_origin(&parsed, policy)?;
    let host = parsed
        .host_str()
        .ok_or_else(|| invalid_request("OpenAI-compatible base_url must include a host"))?;
    // `host_str()` returns bracketed IPv6 (`[::1]`); strip the brackets before
    // the literal check so an IPv6 literal is classified as a literal (already
    // validated by `validate_compatible_origin` above) rather than misread as a
    // hostname and sent through a request-time DNS lookup that fails. A domain
    // never contains brackets, so the trim is a no-op for it. Fail-closed either
    // way — this is a functional fix, not a security boundary (W5a.2 confirm P2).
    let host_literal = host.trim_start_matches('[').trim_end_matches(']');
    let origin = if host_literal.parse::<IpAddr>().is_ok() {
        None
    } else {
        let port = parsed
            .port_or_known_default()
            .ok_or_else(|| invalid_request("OpenAI-compatible base_url must include a port"))?;
        Some(CompatibleHostnameOrigin {
            host: host.to_owned(),
            port,
            plain_http: parsed.scheme() == "http",
        })
    };
    let api_root = if base_url.ends_with("/v1") {
        base_url.to_owned()
    } else {
        format!("{base_url}/v1")
    };
    Ok(CompatibleEndpoints {
        base_url: base_url.to_owned(),
        chat_url: format!("{api_root}/chat/completions"),
        models_url: format!("{api_root}/models"),
        origin,
    })
}

/// Whether an origin is an Azure OpenAI resource endpoint (G4b): https on
/// `{resource}.openai.azure.com` or `{resource}.services.ai.azure.com` with
/// a non-empty resource label. ONE predicate carries every Azure decision —
/// the `api-key` header mode, the strict origin policy, and the
/// configured-deployment availability fallback — so they can never disagree.
#[must_use]
pub fn azure_openai_origin(origin: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(origin.trim()) else {
        return false;
    };
    if parsed.scheme() != "https"
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return false;
    }
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    ["openai.azure.com", "services.ai.azure.com"]
        .iter()
        .any(|suffix| {
            host.strip_suffix(suffix)
                .and_then(|prefix| prefix.strip_suffix('.'))
                .is_some_and(|resource| {
                    !resource.is_empty()
                        && resource
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                })
        })
}

/// Validates and probes a configured OpenAI-compatible endpoint without
/// attaching credential material.
///
/// This is the provider-registry entry point for W5 management. It reuses the
/// adapter's URL, SSRF, DNS pinning, timeout, and no-redirect policy so the
/// daemon cannot grow a second, weaker endpoint validator.
pub async fn validate_openai_compatible_endpoint(
    base_url: &str,
    policy: CompatibleOriginPolicy,
) -> Result<String, ProviderError> {
    let endpoints = compatible_endpoints(base_url, policy)?;
    let mut client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(OPENAI_DEFAULT_TRANSPORT_CONFIG.connect_timeout);
    let guard = endpoints.origin.map(|origin| {
        Arc::new(CompatibleOriginGuard::new(
            origin.host,
            origin.port,
            origin.plain_http,
            policy,
            Arc::new(SystemFixedDnsResolver),
        ))
    });
    if let Some(guard) = &guard {
        connect_before_deadline(
            OPENAI_DEFAULT_TRANSPORT_CONFIG.connect_timeout,
            guard.validate(),
        )
        .await?;
        client = client.dns_resolver(Arc::clone(guard));
    }
    let client = client.build().map_err(|error| {
        ProviderError::new(
            ProviderErrorKind::Internal,
            format!("could not construct OpenAI-compatible endpoint validator: {error}"),
        )
    })?;
    let opening = client
        .get(&endpoints.models_url)
        .header(ACCEPT, "application/json")
        .send();
    let response = response_before_deadline(
        OPENAI_DEFAULT_TRANSPORT_CONFIG.response_open_timeout,
        async {
            opening.await.map_err(|error| {
                transport_error_with_config(error, OPENAI_DEFAULT_TRANSPORT_CONFIG)
            })
        },
    )
    .await?;
    if response.status().is_redirection() {
        return Err(invalid_request(
            "OpenAI-compatible endpoint redirects are not allowed; configure the final origin",
        ));
    }
    Ok(endpoints.base_url)
}

fn validate_compatible_origin(
    parsed: &reqwest::Url,
    policy: CompatibleOriginPolicy,
) -> Result<(), ProviderError> {
    let host = parsed
        .host_str()
        .ok_or_else(|| invalid_request("OpenAI-compatible base_url must include a host"))?;
    let ip = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host)
        .parse::<IpAddr>()
        .ok();
    if ip.is_some_and(|address| blocked_credential_target_with_policy(address, policy)) {
        return Err(invalid_request(blocked_target_message(policy)));
    }
    if parsed.scheme() == "http" && ip.is_some_and(|address| !plain_http_allowed(address, policy)) {
        return Err(invalid_request(plain_http_message(policy)));
    }
    Ok(())
}

fn blocked_target_message(policy: CompatibleOriginPolicy) -> &'static str {
    match policy {
        CompatibleOriginPolicy::Strict => {
            "OpenAI-compatible base_url must not target a private, link-local, or special-use IP address"
        }
        CompatibleOriginPolicy::TrustedLan => {
            "OpenAI-compatible base_url must not target a link-local, multicast, or special-use IP address"
        }
    }
}

fn plain_http_message(policy: CompatibleOriginPolicy) -> &'static str {
    match policy {
        CompatibleOriginPolicy::Strict => {
            "OpenAI-compatible remote base_url must use HTTPS; HTTP is allowed only for loopback addresses"
        }
        CompatibleOriginPolicy::TrustedLan => {
            "OpenAI-compatible remote base_url must use HTTPS; HTTP is allowed only for loopback or RFC1918 LAN addresses"
        }
    }
}

struct CompatibleOriginGuard {
    host: String,
    port: u16,
    plain_http: bool,
    policy: CompatibleOriginPolicy,
    resolver: Arc<dyn FixedDnsResolver>,
    validated: OnceCell<Result<Arc<[SocketAddr]>, ProviderError>>,
    #[cfg(test)]
    connection_lookups: AtomicUsize,
}

/// Opaque custom-origin resolver shared with the custom Anthropic adapter.
/// The underlying resolver and its injection seam remain private to this
/// module, so no private trait leaks through a public(crate) signature.
pub(crate) struct CustomCompatibleOriginGuard(CompatibleOriginGuard);

impl CustomCompatibleOriginGuard {
    pub(crate) fn for_base_url(base_url: &str) -> Result<(String, Arc<Self>), ProviderError> {
        let endpoints = compatible_endpoints(base_url, CompatibleOriginPolicy::TrustedLan)?;
        let parsed = reqwest::Url::parse(&endpoints.base_url)
            .map_err(|_| invalid_request("OpenAI-compatible base_url is not a valid URL"))?;
        let host = parsed
            .host_str()
            .ok_or_else(|| invalid_request("OpenAI-compatible base_url must include a host"))?
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned();
        let port = parsed
            .port_or_known_default()
            .ok_or_else(|| invalid_request("OpenAI-compatible base_url must include a port"))?;
        let guard = Arc::new(Self(CompatibleOriginGuard::new(
            host,
            port,
            parsed.scheme() == "http",
            CompatibleOriginPolicy::TrustedLan,
            Arc::new(SystemFixedDnsResolver),
        )));
        Ok((endpoints.base_url, guard))
    }

    pub(crate) async fn validate_endpoint(&self, endpoint: &str) -> Result<(), ProviderError> {
        self.0.validate_endpoint(endpoint).await
    }
}

impl fmt::Debug for CustomCompatibleOriginGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl reqwest::dns::Resolve for CustomCompatibleOriginGuard {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        self.0.resolve(name)
    }
}

struct PinnedAddrs {
    addresses: Arc<[SocketAddr]>,
    next: usize,
}

impl Iterator for PinnedAddrs {
    type Item = SocketAddr;

    fn next(&mut self) -> Option<Self::Item> {
        let address = self.addresses.get(self.next).copied();
        self.next += usize::from(address.is_some());
        address
    }
}

impl CompatibleOriginGuard {
    fn new(
        host: String,
        port: u16,
        plain_http: bool,
        policy: CompatibleOriginPolicy,
        resolver: Arc<dyn FixedDnsResolver>,
    ) -> Self {
        Self {
            host,
            port,
            plain_http,
            policy,
            resolver,
            validated: OnceCell::new(),
            #[cfg(test)]
            connection_lookups: AtomicUsize::new(0),
        }
    }

    async fn validate(&self) -> Result<(), ProviderError> {
        self.validated_addresses().await.map(|_| ())
    }

    async fn validate_endpoint(&self, endpoint: &str) -> Result<(), ProviderError> {
        let parsed = reqwest::Url::parse(endpoint).map_err(|_| {
            invalid_request("credential-bearing compatible endpoint is not a valid URL")
        })?;
        let expected_scheme = if self.plain_http { "http" } else { "https" };
        if parsed.scheme() != expected_scheme
            || !parsed
                .host_str()
                .map(|host| host.trim_start_matches('[').trim_end_matches(']'))
                .is_some_and(|host| host.eq_ignore_ascii_case(&self.host))
            || parsed.port_or_known_default() != Some(self.port)
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return Err(invalid_request(
                "credential-bearing compatible request left its pinned origin",
            ));
        }
        self.validate().await
    }

    async fn validated_addresses(&self) -> Result<Arc<[SocketAddr]>, ProviderError> {
        self.validated
            .get_or_init(|| async {
                let addresses = self.resolver.resolve(&self.host, self.port).await.map_err(
                    |error| {
                        ProviderError::new(
                            ProviderErrorKind::Transport,
                            format!(
                                "could not resolve OpenAI-compatible base_url host `{}`: {error}",
                                self.host
                            ),
                        )
                    },
                )?;
                validate_resolved_compatible_origin(
                    &self.host,
                    self.plain_http,
                    self.policy,
                    addresses,
                )
            })
            .await
            .clone()
    }
}

impl fmt::Debug for CompatibleOriginGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompatibleOriginGuard")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("plain_http", &self.plain_http)
            .field("validated", &self.validated.get().is_some())
            .finish_non_exhaustive()
    }
}

impl reqwest::dns::Resolve for CompatibleOriginGuard {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        #[cfg(test)]
        self.connection_lookups.fetch_add(1, Ordering::SeqCst);
        let requested = name.as_str();
        let result: Result<
            reqwest::dns::Addrs,
            Box<dyn std::error::Error + Send + Sync + 'static>,
        > = if !requested
            .trim_end_matches('.')
            .eq_ignore_ascii_case(self.host.trim_end_matches('.'))
        {
            Err(Box::new(io::Error::other(format!(
                "pinned OpenAI-compatible resolver refused unexpected host `{requested}`"
            ))))
        } else {
            match self.validated.get() {
                Some(Ok(addresses)) => Ok(Box::new(PinnedAddrs {
                    addresses: Arc::clone(addresses),
                    next: 0,
                })),
                Some(Err(error)) => Err(Box::new(io::Error::other(error.message.clone()))),
                None => Err(Box::new(io::Error::other(
                    "OpenAI-compatible origin was not validated before connection",
                ))),
            }
        };
        Box::pin(std::future::ready(result))
    }
}

fn validate_resolved_compatible_origin(
    host: &str,
    plain_http: bool,
    policy: CompatibleOriginPolicy,
    addresses: Vec<SocketAddr>,
) -> Result<Arc<[SocketAddr]>, ProviderError> {
    if addresses.is_empty() {
        return Err(invalid_request(format!(
            "OpenAI-compatible base_url host `{host}` resolved to no addresses"
        )));
    }

    let mut pinned = Vec::with_capacity(addresses.len());
    for address in addresses {
        if blocked_credential_target_with_policy(address.ip(), policy) {
            return Err(invalid_request(format!(
                "OpenAI-compatible base_url host `{host}` resolved to a forbidden IP address ({})",
                blocked_target_message(policy)
            )));
        }
        if plain_http && !plain_http_allowed(address.ip(), policy) {
            return Err(invalid_request(format!(
                "OpenAI-compatible remote base_url must use HTTPS; HTTP host `{host}` resolved to a non-loopback address"
            )));
        }
        if !pinned.contains(&address) {
            pinned.push(address);
        }
    }
    Ok(pinned.into())
}

pub(crate) fn blocked_credential_target(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => blocked_ipv4_credential_target(address),
        IpAddr::V6(address) => blocked_ipv6_credential_target(address),
    }
}

/// Policy-aware fence: `TrustedLan` exempts exactly the RFC1918 ranges from
/// the strict block list; everything else the strict fence refuses (link-
/// local, multicast, unspecified, broadcast, ULA, 0/8) stays refused.
pub(crate) fn blocked_credential_target_with_policy(
    address: IpAddr,
    policy: CompatibleOriginPolicy,
) -> bool {
    match policy {
        CompatibleOriginPolicy::Strict => blocked_credential_target(address),
        CompatibleOriginPolicy::TrustedLan => {
            blocked_credential_target(address) && !rfc1918_private(address)
        }
    }
}

/// Exactly the RFC1918 ranges (10/8, 172.16/12, 192.168/16), including their
/// IPv4-mapped IPv6 forms. Link-local `169.254.0.0/16` is NOT private here.
pub(crate) fn rfc1918_private(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => address.is_private(),
        IpAddr::V6(address) => address
            .to_ipv4_mapped()
            .is_some_and(|mapped| mapped.is_private()),
    }
}

fn plain_http_allowed(address: IpAddr, policy: CompatibleOriginPolicy) -> bool {
    address.is_loopback()
        || (policy == CompatibleOriginPolicy::TrustedLan && rfc1918_private(address))
}

fn blocked_ipv4_credential_target(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    address.is_private()
        || address.is_link_local()
        || address.is_unspecified()
        || address.is_multicast()
        || address == Ipv4Addr::BROADCAST
        || octets[0] == 0
        // M5 (W-F): CGNAT / RFC 6598 shared address space 100.64.0.0/10 —
        // routable inside carrier NATs and home cloud metadata twins.
        || (octets[0] == 100 && (octets[1] & 0xc0) == 0x40)
        // M5: RFC 2544 benchmarking range 198.18.0.0/15.
        || (octets[0] == 198 && (octets[1] & 0xfe) == 18)
        // M5: IETF protocol assignments 192.0.0.0/24 (special-use).
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
        // M5: reserved / Class E 240.0.0.0/4 (240.0.0.0 – 255.255.255.255).
        || octets[0] >= 240
}

fn blocked_ipv6_credential_target(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    address
        .to_ipv4_mapped()
        .is_some_and(blocked_ipv4_credential_target)
        || address.is_unspecified()
        || address.is_multicast()
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] & 0xfe00) == 0xfc00
        // M5 (W-F): NAT64 well-known prefix 64:ff9b::/96 — an embedded IPv4
        // (e.g. 64:ff9b::7f00:1 == 127.0.0.1) must never dodge the fence.
        || (segments[0] == 0x0064
            && segments[1] == 0xff9b
            && segments[2] == 0
            && segments[3] == 0
            && segments[4] == 0
            && segments[5] == 0)
}

#[derive(Debug, Deserialize)]
struct ModelsEnvelope {
    data: Vec<ModelEntry>,
}

#[derive(Debug, Deserialize)]
struct ModelEntry {
    id: String,
}

fn openai_usage(
    value: &serde_json::Value,
    account: Option<CredentialAlias>,
) -> Result<Usage, ProviderError> {
    let input = required_u64(value, "input_tokens", "OpenAI usage")?;
    let output = required_u64(value, "output_tokens", "OpenAI usage")?;
    let reasoning = value
        .get("output_tokens_details")
        .and_then(|details| details.get("reasoning_tokens"))
        .and_then(serde_json::Value::as_u64);
    let details = value.get("input_tokens_details");
    let cached = details
        .and_then(|details| details.get("cached_tokens"))
        .and_then(serde_json::Value::as_u64);
    let cache_write = details
        .and_then(|details| {
            details
                .get("cache_write_tokens")
                .or_else(|| details.get("created_cache_tokens"))
        })
        .and_then(serde_json::Value::as_u64);
    let normalized = subset_cache_usage(input, cached, cache_write, output, reasoning);
    Ok(Usage {
        input,
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

fn chat_usage(
    value: &serde_json::Value,
    account: Option<CredentialAlias>,
    dialect: CompatibleDialect,
) -> Result<Usage, ProviderError> {
    let prompt_tokens = required_u64(value, "prompt_tokens", "Chat usage")?;
    let output = required_u64(value, "completion_tokens", "Chat usage")?;
    let reasoning = value
        .get("completion_tokens_details")
        .and_then(|details| details.get("reasoning_tokens"))
        .and_then(serde_json::Value::as_u64);
    // DeepSeek reports disjoint top-level hit/miss counters. Pass-through
    // proxies retain that upstream shape, so the fields that actually arrived
    // select this decoder regardless of the locally configured dialect.
    // Direct DeepSeek remains strict even when both fields are absent.
    let has_deepseek_cache_shape = value.get("prompt_cache_miss_tokens").is_some()
        || value.get("prompt_cache_hit_tokens").is_some();
    let deepseek_miss = value
        .get("prompt_cache_miss_tokens")
        .and_then(serde_json::Value::as_u64);
    let deepseek_hit = value
        .get("prompt_cache_hit_tokens")
        .and_then(serde_json::Value::as_u64);
    let parse_deepseek_cache_shape =
        dialect == CompatibleDialect::DeepSeekApi || has_deepseek_cache_shape;
    let (input, normalized) = if parse_deepseek_cache_shape {
        // Once either DeepSeek field is present, the pair is authoritative
        // only when BOTH values are valid and reconcile to the prompt total.
        // Partial or malformed telemetry is unavailable and must not fall back
        // to another shape, be saturated, or acquire a synthetic zero.
        match (deepseek_miss, deepseek_hit) {
            (Some(miss), Some(hit)) if miss.checked_add(hit) == Some(prompt_tokens) => (
                miss,
                normalized_cache_usage(
                    prompt_tokens,
                    miss,
                    hit,
                    None,
                    output,
                    reasoning,
                    CacheStatAvailability::Present,
                ),
            ),
            _ => (
                prompt_tokens,
                unavailable_cache_usage(prompt_tokens, output, reasoning),
            ),
        }
    } else {
        let details = value.get("prompt_tokens_details");
        let kimi_cached = value.get("cached_tokens");
        let cached = if let Some(cached) = kimi_cached {
            // Kimi's native counter is top-level. Proxies preserve that shape,
            // so field presence selects it regardless of the local dialect.
            // A malformed present field is authoritative and cannot fall
            // through to a nested counter.
            cached.as_u64()
        } else {
            // Kimi also accepts/returns the OpenAI-compatible nested shape;
            // absence of the native field permits that recognized fallback.
            details
                .and_then(|details| details.get("cached_tokens"))
                .and_then(serde_json::Value::as_u64)
        };
        let cache_write = if kimi_cached.is_some() {
            None
        } else {
            value
                .get("cache_write_tokens")
                .or_else(|| value.get("created_cache_tokens"))
                .and_then(serde_json::Value::as_u64)
                .or_else(|| {
                    details
                        .and_then(|details| {
                            details
                                .get("cache_write_tokens")
                                .or_else(|| details.get("created_cache_tokens"))
                        })
                        .and_then(serde_json::Value::as_u64)
                })
        };
        (
            prompt_tokens,
            subset_cache_usage(prompt_tokens, cached, cache_write, output, reasoning),
        )
    };
    Ok(Usage {
        input,
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

fn unavailable_cache_usage(
    logical_input: u64,
    billed_output: u64,
    reasoning: Option<u64>,
) -> NormalizedUsage {
    NormalizedUsage {
        logical_input,
        uncached_input: logical_input,
        billed_output,
        reasoning_detail: reasoning.unwrap_or(0),
        reasoning_accounting: reasoning.map_or(ReasoningAccounting::Unavailable, |_| {
            ReasoningAccounting::SubsetOfOutput
        }),
        ..NormalizedUsage::default()
    }
}

fn subset_cache_usage(
    logical_input: u64,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
    billed_output: u64,
    reasoning: Option<u64>,
) -> NormalizedUsage {
    let Some(cache_read) = cache_read.filter(|cached| *cached <= logical_input) else {
        return unavailable_cache_usage(logical_input, billed_output, reasoning);
    };
    let uncached = logical_input - cache_read;
    normalized_cache_usage(
        logical_input,
        uncached,
        cache_read,
        cache_write,
        billed_output,
        reasoning,
        CacheStatAvailability::Present,
    )
}

#[allow(clippy::too_many_arguments)]
fn normalized_cache_usage(
    logical_input: u64,
    uncached_input: u64,
    cache_read_input: u64,
    cache_write: Option<u64>,
    billed_output: u64,
    reasoning: Option<u64>,
    cache_status: CacheStatAvailability,
) -> NormalizedUsage {
    let (cache_write_input, cache_write_status) = match cache_write {
        Some(write) if write <= uncached_input => (write, CacheStatAvailability::Present),
        _ => (0, CacheStatAvailability::Unavailable),
    };
    NormalizedUsage {
        logical_input,
        uncached_input,
        cache_read_input,
        cache_write_input,
        billed_output,
        reasoning_detail: reasoning.unwrap_or(0),
        reasoning_accounting: reasoning.map_or(ReasoningAccounting::Unavailable, |_| {
            ReasoningAccounting::SubsetOfOutput
        }),
        cache_status,
        cache_write_status,
        cache_telemetry_input: if cache_status == CacheStatAvailability::Present {
            logical_input
        } else {
            0
        },
        ..NormalizedUsage::default()
    }
}

fn required_string(
    value: &serde_json::Value,
    field: &str,
    event: &str,
) -> Result<String, ProviderError> {
    value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| malformed(format!("OpenAI {event} has no string `{field}`")))
}

fn object_string(
    value: &serde_json::Map<String, serde_json::Value>,
    field: &str,
    event: &str,
) -> Result<String, ProviderError> {
    value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| malformed(format!("OpenAI {event} has no string `{field}`")))
}

fn required_u64(value: &serde_json::Value, field: &str, event: &str) -> Result<u64, ProviderError> {
    value
        .get(field)
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| malformed(format!("{event} has no integer `{field}`")))
}

fn required_usize(
    value: &serde_json::Value,
    field: &str,
    event: &str,
) -> Result<usize, ProviderError> {
    let integer = required_u64(value, field, event)?;
    usize::try_from(integer)
        .map_err(|_| malformed(format!("{event} `{field}` does not fit in usize")))
}

fn normalize_chat_finish_reason(reason: &str) -> Result<FinishReason, ProviderError> {
    match reason {
        "stop" => Ok(FinishReason::EndTurn),
        "tool_calls" | "function_call" => Ok(FinishReason::ToolUse),
        "length" => Ok(FinishReason::MaxTokens),
        "content_filter" => Ok(FinishReason::Refusal),
        other => Err(malformed(format!(
            "OpenAI-compatible Chat returned unknown finish_reason `{other}`"
        ))),
    }
}

fn transport_error(error: reqwest::Error) -> ProviderError {
    crate::reqwest_transport_error("OpenAI", error)
}

fn transport_error_with_config(
    error: reqwest::Error,
    transport_config: OpenAiTransportConfig,
) -> ProviderError {
    let connect_timeout_fired = error.is_timeout() && error.is_connect();
    let mut error = transport_error(error);
    if connect_timeout_fired {
        let budget_ms = duration_ms(transport_config.connect_timeout);
        error.message = format!(
            "OpenAI connection did not open within the configured connect budget; opened_within_ms={budget_ms} budget_ms={budget_ms}"
        );
        error.presentation = crate::provider_timeout_presentation();
        error = error.with_timeout_budget(budget_ms, budget_ms);
        error.retryable = full_request_budget_fits(transport_config.connect_timeout);
    }
    error
}

fn validate_transport_config(config: OpenAiTransportConfig) -> Result<(), ProviderError> {
    if config.connect_timeout.is_zero()
        || config.response_open_timeout.is_zero()
        || config.chunk_idle_timeout.is_zero()
    {
        return Err(invalid_request(
            "OpenAI transport timeout budgets must be greater than zero",
        ));
    }
    Ok(())
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

async fn response_before_deadline<T>(
    timeout: Duration,
    opening: impl Future<Output = Result<T, ProviderError>>,
) -> Result<T, ProviderError> {
    let selected = request_phase_budget(timeout)?;
    let started = tokio::time::Instant::now();
    match tokio::time::timeout(selected, opening).await {
        Ok(result) => result,
        Err(_) if selected < timeout => {
            Err(crate::deadline_exhausted_error(selected, started.elapsed()))
        }
        Err(_) => {
            let mut error = response_open_timeout_error(timeout, started.elapsed());
            error.retryable = full_request_budget_fits(timeout);
            Err(error)
        }
    }
}

async fn connect_before_deadline<T>(
    timeout: Duration,
    connecting: impl Future<Output = Result<T, ProviderError>>,
) -> Result<T, ProviderError> {
    let selected = request_phase_budget(timeout)?;
    let started = tokio::time::Instant::now();
    match tokio::time::timeout(selected, connecting).await {
        Ok(result) => result,
        Err(_) if selected < timeout => {
            Err(crate::deadline_exhausted_error(selected, started.elapsed()))
        }
        Err(_) => {
            let mut error = connect_timeout_error(timeout);
            error.retryable = full_request_budget_fits(timeout);
            Err(error)
        }
    }
}

fn request_phase_budget(configured: Duration) -> Result<Duration, ProviderError> {
    crate::effective_request_budget(
        configured,
        crate::current_provider_deadline_remaining(),
        crate::PROVIDER_DEADLINE_SAFETY_MARGIN,
    )
    .map_err(|crate::ProviderTimeoutReason::DeadlineExhausted| {
        crate::deadline_exhausted_error(Duration::ZERO, Duration::ZERO)
    })
}

fn full_request_budget_fits(configured: Duration) -> bool {
    let remaining = crate::current_provider_deadline_remaining();
    crate::effective_request_budget(
        configured,
        remaining,
        crate::PROVIDER_DEADLINE_SAFETY_MARGIN,
    )
    .is_ok_and(|selected| selected == configured)
}

fn connect_timeout_error(timeout: Duration) -> ProviderError {
    let budget_ms = duration_ms(timeout);
    ProviderError::new(
        ProviderErrorKind::Transport,
        format!(
            "OpenAI connection preflight did not finish within the configured connect budget; opened_within_ms={budget_ms} budget_ms={budget_ms}"
        ),
    )
    .with_presentation(crate::provider_timeout_presentation())
    .with_timeout_budget(budget_ms, budget_ms)
}

fn response_open_timeout_error(timeout: Duration, elapsed: Duration) -> ProviderError {
    let opened_within_ms = duration_ms(elapsed.min(timeout));
    let budget_ms = duration_ms(timeout);
    ProviderError::new(
        ProviderErrorKind::Transport,
        format!(
            "OpenAI response did not open within the configured response-open budget; opened_within_ms={opened_within_ms} budget_ms={budget_ms}"
        ),
    )
    .with_presentation(crate::provider_timeout_presentation())
    .with_timeout_budget(opened_within_ms, budget_ms)
}

fn stream_idle_error(timeout: Duration) -> ProviderError {
    let budget_ms = duration_ms(timeout);
    ProviderError::new(
        ProviderErrorKind::Transport,
        format!(
            "OpenAI SSE stream received no data within the configured idle budget; opened_within_ms={budget_ms} budget_ms={budget_ms}"
        ),
    )
    .with_presentation(crate::provider_timeout_presentation())
    .with_timeout_budget(budget_ms, budget_ms)
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

#[cfg(test)]
#[path = "openai_tests.rs"]
mod tests;
