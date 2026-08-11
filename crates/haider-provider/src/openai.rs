//! OpenAI Responses API and OpenAI-compatible Chat Completions adapters.
//!
//! The native adapter uses Responses because its typed output-item stream maps
//! directly to Haider's text, reasoning-summary, tool-call, usage, and finish
//! events. The compatible adapter reuses the same transport policy but speaks
//! Chat Completions, the common wire implemented by vLLM, Ollama, LM Studio,
//! LiteLLM, TGI, Hugging Face endpoints, and generic gateways.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use haider_accounts::SecretHandle;
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
pub const OPENAI_RESPONSES_API_URL: &str = "https://api.openai.com/v1/responses";
pub const OPENAI_SUBSCRIPTION_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
pub const OPENAI_SUBSCRIPTION_RESPONSES_URL: &str =
    "https://chatgpt.com/backend-api/codex/responses";
pub const OPENAI_CODEX_RESPONSES_LITE_HEADER: &str = "x-openai-internal-codex-responses-lite";
pub const OPENAI_CODEX_RESPONSES_LITE_VALUE: &str = "true";
/// W-B (decision 3): the UNOFFICIAL codex search endpoint the client
/// `web_search` tool executes against on lite pairs — same origin and
/// Bearer as subscription turns. Source-verified against codex main
/// 2026-08; a 404/410 degrades the capability for the session.
pub const OPENAI_ALPHA_SEARCH_URL: &str = "https://chatgpt.com/backend-api/codex/alpha/search";
const OPENAI_SUBSCRIPTION_HOST: &str = "chatgpt.com";
const KIMI_OAUTH_HOST: &str = "api.kimi.com";
const DEEPSEEK_HOST: &str = "api.deepseek.com";

const STREAM_CAPACITY: usize = 32;
const MODELS_BODY_LIMIT: usize = 1024 * 1024;
const ERROR_BODY_LIMIT: usize = 64 * 1024;
const TRANSPORT_CONFIG: OpenAiTransportConfig = OpenAiTransportConfig {
    retry_policy: OpenAiRetryPolicy::Never,
    connect_timeout: Duration::from_secs(10),
    response_open_timeout: Duration::from_secs(30),
    chunk_idle_timeout: Duration::from_secs(90),
};

/// Retry behavior owned by the OpenAI HTTP adapters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiRetryPolicy {
    /// Surface each transport failure once; the actor owns retry/backoff.
    Never,
}

/// Inspectable transport invariants shared by native and compatible adapters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenAiTransportConfig {
    pub retry_policy: OpenAiRetryPolicy,
    pub connect_timeout: Duration,
    pub response_open_timeout: Duration,
    pub chunk_idle_timeout: Duration,
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
}

impl OpenAiHttp {
    fn new(credential: SecretHandle, model: impl Into<String>) -> Result<Self, ProviderError> {
        Self::new_with_origin_guards(credential, model, None, None, false)
    }

    fn new_with_origin_guard(
        credential: SecretHandle,
        model: impl Into<String>,
        origin_guard: Option<Arc<CompatibleOriginGuard>>,
    ) -> Result<Self, ProviderError> {
        Self::new_with_origin_guards(credential, model, origin_guard, None, false)
    }

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

    fn new_with_origin_guards(
        credential: SecretHandle,
        model: impl Into<String>,
        origin_guard: Option<Arc<CompatibleOriginGuard>>,
        fixed_origin_guard: Option<Arc<FixedOriginGuard>>,
        codex_responses_lite: bool,
    ) -> Result<Self, ProviderError> {
        let transport = TRANSPORT_CONFIG;
        let mut client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(match transport.retry_policy {
                OpenAiRetryPolicy::Never => reqwest::retry::never(),
            })
            .connect_timeout(transport.connect_timeout);
        if let Some(guard) = &origin_guard {
            client = client.dns_resolver(Arc::clone(guard));
        }
        if let Some(guard) = &fixed_origin_guard {
            client = client.dns_resolver(Arc::clone(guard));
        }
        let client = client.build().map_err(|error| {
            ProviderError::new(
                ProviderErrorKind::Internal,
                format!("could not construct OpenAI HTTP client: {error}"),
            )
        })?;
        Ok(Self {
            client,
            credential,
            account: None,
            model: model.into(),
            origin_guard,
            fixed_origin_guard,
            codex_responses_lite,
            auth_header_mode: OpenAiAuthHeaderMode::Bearer,
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
        })
    }

    async fn post_json(
        &self,
        url: &str,
        payload: &serde_json::Value,
    ) -> Result<reqwest::Response, ProviderError> {
        let opening = async {
            let request = self.post_json_request(url, payload).await?;
            self.client.execute(request).await.map_err(transport_error)
        };
        tokio::time::timeout(TRANSPORT_CONFIG.response_open_timeout, opening)
            .await
            .map_err(|_| response_open_timeout_error(TRANSPORT_CONFIG.response_open_timeout))?
    }

    async fn post_json_request(
        &self,
        url: &str,
        payload: &serde_json::Value,
    ) -> Result<reqwest::Request, ProviderError> {
        self.validate_origin(url).await?;
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
        request.json(payload).build().map_err(transport_error)
    }

    async fn get(&self, url: &str) -> Result<reqwest::Response, ProviderError> {
        let opening = async {
            let request = self.get_request(url).await?;
            self.client.execute(request).await.map_err(transport_error)
        };
        tokio::time::timeout(TRANSPORT_CONFIG.response_open_timeout, opening)
            .await
            .map_err(|_| response_open_timeout_error(TRANSPORT_CONFIG.response_open_timeout))?
    }

    async fn get_request(&self, url: &str) -> Result<reqwest::Request, ProviderError> {
        self.validate_origin(url).await?;
        self.with_auth_header(self.client.get(url).header(ACCEPT, "application/json"))?
            .build()
            .map_err(transport_error)
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

    #[cfg(test)]
    async fn validate_compatible_origin(&self) -> Result<(), ProviderError> {
        match &self.origin_guard {
            Some(guard) => guard.validate().await,
            None => Ok(()),
        }
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
        Self::new_subscription_with_dns_resolver(
            credential,
            model,
            base_url,
            Arc::new(SystemFixedDnsResolver),
        )
    }

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
        TRANSPORT_CONFIG
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
        let payload = self.request_payload(request)?;
        self.http.post_json(&self.api_url, &payload).await
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

    async fn capabilities(&self) -> CapabilityDoc {
        native_capabilities(&self.http.model)
    }

    async fn stream_turn(&self, request: TurnRequest) -> Result<ProviderStream, ProviderError> {
        let response = self.send_request(&request).await?;
        checked_stream(response, self.http.account.clone(), DecoderKind::Responses).await
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
        Self::new_with_dns_resolver(
            credential,
            model,
            base_url,
            Arc::new(SystemCompatibleDnsResolver),
        )
    }

    /// Constructs the CUSTOM-provenance adapter under
    /// [`CompatibleOriginPolicy::TrustedLan`] (G4a): RFC1918 LAN origins are
    /// valid over http and https; the rest of the origin fence is unchanged.
    pub fn new_custom(
        credential: SecretHandle,
        model: impl Into<String>,
        base_url: impl AsRef<str>,
    ) -> Result<Self, ProviderError> {
        Self::new_with_policy_and_dns_resolver(
            credential,
            model,
            base_url,
            CompatibleOriginPolicy::TrustedLan,
            Arc::new(SystemCompatibleDnsResolver),
        )
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
        let mut provider = Self::new_with_policy_and_dns_resolver(
            credential,
            model,
            base_url,
            CompatibleOriginPolicy::Strict,
            Arc::new(SystemCompatibleDnsResolver),
        )?;
        provider.http.auth_header_mode = OpenAiAuthHeaderMode::AzureApiKey;
        Ok(provider)
    }

    fn new_with_dns_resolver(
        credential: SecretHandle,
        model: impl Into<String>,
        base_url: impl AsRef<str>,
        resolver: Arc<dyn CompatibleDnsResolver>,
    ) -> Result<Self, ProviderError> {
        Self::new_with_policy_and_dns_resolver(
            credential,
            model,
            base_url,
            CompatibleOriginPolicy::Strict,
            resolver,
        )
    }

    fn new_with_policy_and_dns_resolver(
        credential: SecretHandle,
        model: impl Into<String>,
        base_url: impl AsRef<str>,
        policy: CompatibleOriginPolicy,
        resolver: Arc<dyn CompatibleDnsResolver>,
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
        Self::new_kimi_subscription_with_dns_resolver(
            credential,
            model,
            base_url,
            Arc::new(SystemFixedDnsResolver),
        )
    }

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
        Self::new_deepseek_api_with_dns_resolver(
            credential,
            model,
            base_url,
            Arc::new(SystemFixedDnsResolver),
        )
    }

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
        TRANSPORT_CONFIG
    }

    #[must_use]
    pub fn with_account(mut self, account: CredentialAlias) -> Self {
        self.http.account = Some(account);
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

    /// Probes `GET /models` and derives only conservative capabilities from
    /// the presence and name of the configured model. The models endpoint does
    /// not itself prove tool/vision/reasoning support.
    pub async fn probe_capabilities(&self) -> Result<CapabilityDoc, ProviderError> {
        let response = self.http.get(&self.models_url).await?;
        if !response.status().is_success() {
            return Err(http_error_from_response(response).await);
        }
        let body =
            read_body_bounded(response, MODELS_BODY_LIMIT, "OpenAI-compatible /models").await?;
        match self.dialect {
            CompatibleDialect::Generic => replay_openai_models_response(&self.http.model, &body),
            CompatibleDialect::KimiOAuth => replay_kimi_models_response(&self.http.model, &body),
            CompatibleDialect::DeepSeekApi => {
                replay_deepseek_models_response(&self.http.model, &body)
            }
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
        let payload = self.request_payload(request)?;
        self.http.post_json(&self.chat_url, &payload).await
    }
}

#[async_trait]
impl Provider for OpenAiCompatibleProvider {
    fn credential_surface(&self) -> crate::ProviderCredentialSurface {
        match self.dialect {
            CompatibleDialect::Generic | CompatibleDialect::DeepSeekApi => {
                crate::ProviderCredentialSurface::ApiKey
            }
            CompatibleDialect::KimiOAuth => {
                crate::ProviderCredentialSurface::OAuthSubscriptionBearer
            }
        }
    }

    async fn capabilities(&self) -> CapabilityDoc {
        let provider = match self.dialect {
            CompatibleDialect::Generic => OPENAI_COMPATIBLE_PROVIDER_NAME,
            CompatibleDialect::KimiOAuth => KIMI_OAUTH_PROVIDER_NAME,
            CompatibleDialect::DeepSeekApi => DEEPSEEK_PROVIDER_NAME,
        };
        self.probe_capabilities()
            .await
            .unwrap_or_else(|_| unavailable_compatible_capabilities(provider))
    }

    async fn stream_turn(&self, request: TurnRequest) -> Result<ProviderStream, ProviderError> {
        let response = self.send_request(&request).await?;
        checked_stream(
            response,
            self.http.account.clone(),
            DecoderKind::Chat(self.dialect),
        )
        .await
    }
}

async fn capture(response: reqwest::Response) -> Result<OpenAiCapture, ProviderError> {
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = response.bytes().await.map_err(transport_error)?.to_vec();
    Ok(OpenAiCapture {
        status,
        retry_after,
        body,
    })
}

async fn checked_stream(
    response: reqwest::Response,
    account: Option<CredentialAlias>,
    decoder: DecoderKind,
) -> Result<ProviderStream, ProviderError> {
    if !response.status().is_success() {
        return Err(http_error_from_response(response).await);
    }
    let (sender, receiver) = mpsc::channel(STREAM_CAPACITY);
    let producer = tokio::spawn(async move {
        stream_response(
            response,
            account,
            sender,
            TRANSPORT_CONFIG.chunk_idle_timeout,
            decoder,
        )
        .await;
    });
    Ok(ProviderStream::owned(receiver, producer))
}

async fn http_error_from_response(response: reqwest::Response) -> ProviderError {
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    match read_body_bounded(response, ERROR_BODY_LIMIT, "OpenAI HTTP error").await {
        Ok(body) => replay_openai_http_error(status, retry_after.as_deref(), &body),
        Err(error) => classify_http_body_read_error(status, retry_after.as_deref(), error),
    }
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
    Responses,
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
        DecoderKind::Responses => OpenAiDecoder::Responses(ResponsesDecoder::new(account)),
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
    fn new(account: Option<CredentialAlias>) -> Self {
        Self {
            framer: SseFramer::default(),
            account,
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
            items.push(Err(malformed(
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
        let value: serde_json::Value = serde_json::from_str(&frame.data).map_err(|error| {
            malformed(format!(
                "OpenAI Responses SSE data is not valid JSON: {error}"
            ))
        })?;
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

/// The exact SearchRequest body the daemon POSTs to
/// [`OPENAI_ALPHA_SEARCH_URL`] for the client `web_search` tool (W-B
/// decision 3, LW4 golden). Locked settings: `search_context_size: medium`,
/// `allowed_callers: ["direct"]`, `external_web_access: true`. `input` stays
/// empty (recent-history seeding is a codex nicety, not part of the locked
/// contract) and `max_output_tokens` is omitted — the same backend bans it
/// on lite.
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
        "commands": [{"type": "search", "query": query}],
        "settings": {
            "search_context_size": "medium",
            "allowed_callers": ["direct"],
            "external_web_access": true,
        },
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
            items.push(Err(malformed(
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
        let value: serde_json::Value = serde_json::from_str(&frame.data).map_err(|error| {
            malformed(format!(
                "OpenAI-compatible Chat SSE data is not valid JSON: {error}"
            ))
        })?;
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
    let mut decoder = ResponsesDecoder::new(None);
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
    let extensions = model
        .extensions
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
    let kind = match status {
        401 => ProviderErrorKind::Authentication,
        403 => ProviderErrorKind::PermissionDenied,
        429 => ProviderErrorKind::RateLimited,
        503 => ProviderErrorKind::Overloaded,
        408 | 500..=599 => ProviderErrorKind::Transport,
        _ if context_exceeded => ProviderErrorKind::ContextExceeded,
        _ => match error_code.or(error_type) {
            Some("invalid_api_key" | "authentication_error") => ProviderErrorKind::Authentication,
            Some("insufficient_quota" | "permission_denied") => ProviderErrorKind::PermissionDenied,
            Some("rate_limit_exceeded" | "rate_limit_error") => ProviderErrorKind::RateLimited,
            Some("server_error" | "timeout") => ProviderErrorKind::Transport,
            _ if parsed
                .as_ref()
                .and_then(|envelope| envelope.error.message.as_deref())
                .is_some_and(|message| message.to_ascii_lowercase().contains("overloaded")) =>
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
    .then(|| parse_retry_after(retry_after))
    .flatten();
    if let Some(detail) = parsed
        .as_ref()
        .and_then(|envelope| envelope.error.message.as_deref())
        .map(str::trim)
        .filter(|message| !message.is_empty())
    {
        let bounded: String = detail.chars().take(200).collect();
        eprintln!("openai http {status} error detail (log-only): {bounded}");
    }
    ProviderError::new(
        kind,
        format!("OpenAI HTTP {status} returned {}", provider_kind_name(kind)),
    )
    .with_retry_after_ms(retry_after_ms)
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
    /// The API's public error prose — the diagnostic that names WHY.
    #[serde(default)]
    message: Option<String>,
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
    let provider_kind = match kind {
        Some("invalid_api_key" | "authentication_error") => ProviderErrorKind::Authentication,
        Some("permission_denied" | "insufficient_quota") => ProviderErrorKind::PermissionDenied,
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
        // The codex backend returns overload under codes the arms above
        // don't know — the prose is the only stable marker. Misclassifying
        // it as InvalidRequest made a RETRYABLE condition error whole runs
        // (nine journaled failures before daemon.log named the cause).
        _ if error
            .get("message")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|message| message.to_ascii_lowercase().contains("overloaded")) =>
        {
            ProviderErrorKind::Overloaded
        }
        _ => ProviderErrorKind::InvalidRequest,
    };
    // The JOURNALED message stays sanitized (the no-leak law: bodies can
    // echo request content). The API's own prose — the diagnostic that
    // names WHY — goes to the owner-local daemon log only (stderr →
    // daemon.log), bounded. Nine sanitized invalid-request failures
    // carried no cause anywhere; this is the middle path.
    if let Some(detail) = error
        .get("message")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|message| !message.is_empty())
    {
        let bounded: String = detail.chars().take(200).collect();
        eprintln!("openai stream error detail (log-only): {bounded}");
    }
    ProviderError::new(
        provider_kind,
        format!(
            "OpenAI stream returned {}",
            provider_kind_name(provider_kind)
        ),
    )
}

fn responses_request_json(
    request: &TurnRequest,
    codex_responses_lite: bool,
    effort: Option<&str>,
    hosted_web_search: bool,
) -> Result<serde_json::Value, ProviderError> {
    let attachments = attachment_index(request)?;
    let mut input = Vec::new();
    for message in &request.messages {
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
                    call_id, preview, ..
                } if matches!(message.role, MessageRole::User | MessageRole::Tool) => {
                    flush_response_message(&mut input, message.role, &mut content);
                    input.push(serde_json::json!({
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": preview,
                    }));
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
    }
    let mut tools = request
        .tools
        .iter()
        .map(|tool| {
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
    if let Some(instructions) = &request.system_prompt {
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
    Ok(payload)
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
    let attachments = attachment_index(request)?;
    let mut messages = Vec::new();
    if let Some(system) = &request.system_prompt {
        messages.push(serde_json::json!({"role": "system", "content": system}));
    }
    for message in &request.messages {
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
                        Block::ToolResult {
                            call_id, preview, ..
                        } => results.push((call_id, preview)),
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
                for (call_id, preview) in results {
                    messages.push(serde_json::json!({
                        "role": "tool",
                        "tool_call_id": call_id,
                        "content": preview,
                    }));
                }
            }
            MessageRole::Tool => {
                for block in &message.blocks {
                    match block {
                        Block::ToolResult {
                            call_id, preview, ..
                        } => messages.push(serde_json::json!({
                            "role": "tool",
                            "tool_call_id": call_id,
                            "content": preview,
                        })),
                        _ => {
                            return Err(invalid_request(
                                "OpenAI-compatible tool messages require tool-result blocks",
                            ));
                        }
                    }
                }
            }
        }
    }
    let tools = request
        .tools
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
        CompatibleDialect::Generic | CompatibleDialect::DeepSeekApi => {
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
        }
    }
    if !tools.is_empty() {
        object.insert("tools".into(), serde_json::Value::Array(tools));
    }
    Ok(payload)
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
        // The models endpoint proves availability only. The generic OpenAI schema
        // carries none of these feature or limit facts, so do not infer them
        // from a vendor-controlled model identifier.
        parallel_tools: FeatureResolve::Unsupported,
        streaming_tool_args: FeatureResolve::Unsupported,
        vision: FeatureResolve::Unsupported,
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
struct CompatibleEndpoints {
    base_url: String,
    chat_url: String,
    models_url: String,
    origin: Option<CompatibleHostnameOrigin>,
}

#[derive(Debug)]
struct CompatibleHostnameOrigin {
    host: String,
    port: u16,
    plain_http: bool,
}

fn compatible_endpoints(
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
    let Some(rest) = origin.trim().strip_prefix("https://") else {
        return false;
    };
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .split(':')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
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
        .connect_timeout(TRANSPORT_CONFIG.connect_timeout);
    let guard = endpoints.origin.map(|origin| {
        Arc::new(CompatibleOriginGuard::new(
            origin.host,
            origin.port,
            origin.plain_http,
            policy,
            Arc::new(SystemCompatibleDnsResolver),
        ))
    });
    if let Some(guard) = &guard {
        guard.validate().await?;
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
    let response = tokio::time::timeout(TRANSPORT_CONFIG.response_open_timeout, opening)
        .await
        .map_err(|_| response_open_timeout_error(TRANSPORT_CONFIG.response_open_timeout))?
        .map_err(transport_error)?;
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

#[async_trait]
trait CompatibleDnsResolver: Send + Sync {
    async fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>>;
}

#[derive(Debug)]
struct SystemCompatibleDnsResolver;

#[async_trait]
impl CompatibleDnsResolver for SystemCompatibleDnsResolver {
    async fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        Ok(tokio::net::lookup_host((host, port)).await?.collect())
    }
}

struct CompatibleOriginGuard {
    host: String,
    port: u16,
    plain_http: bool,
    policy: CompatibleOriginPolicy,
    resolver: Arc<dyn CompatibleDnsResolver>,
    validated: OnceCell<Result<Arc<[SocketAddr]>, ProviderError>>,
    #[cfg(test)]
    connection_lookups: AtomicUsize,
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
        resolver: Arc<dyn CompatibleDnsResolver>,
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
    // DeepSeek reports disjoint top-level hit/miss counters. The pair is
    // authoritative only when BOTH are present and reconcile to the prompt
    // total; a partial or malformed pair is unavailable, never saturated.
    let deepseek_miss = (dialect == CompatibleDialect::DeepSeekApi)
        .then(|| {
            value
                .get("prompt_cache_miss_tokens")
                .and_then(serde_json::Value::as_u64)
        })
        .flatten();
    let deepseek_hit = (dialect == CompatibleDialect::DeepSeekApi)
        .then(|| {
            value
                .get("prompt_cache_hit_tokens")
                .and_then(serde_json::Value::as_u64)
        })
        .flatten();
    let (input, normalized) = if dialect == CompatibleDialect::DeepSeekApi {
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
        let cached = if dialect == CompatibleDialect::KimiOAuth {
            value
                .get("cached_tokens")
                .and_then(serde_json::Value::as_u64)
        } else {
            // Generic OpenAI-compatible endpoints are capability-probed only
            // through the recognized vLLM/OpenAI nested detail shape. An
            // arbitrary top-level `cached_tokens` is not authoritative.
            details
                .and_then(|details| details.get("cached_tokens"))
                .and_then(serde_json::Value::as_u64)
        };
        let cache_write = if dialect == CompatibleDialect::KimiOAuth {
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

fn parse_retry_after(value: Option<&str>) -> Option<u64> {
    let value = value?.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return seconds.checked_mul(1_000);
    }
    let retry_at = httpdate::parse_http_date(value).ok()?;
    let duration = retry_at
        .duration_since(SystemTime::now())
        .unwrap_or_default();
    u64::try_from(duration.as_millis()).ok()
}

fn transport_error(error: reqwest::Error) -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Transport,
        format!("OpenAI HTTP transport failed: {error}"),
    )
}

fn response_open_timeout_error(timeout: Duration) -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Transport,
        format!(
            "OpenAI response did not open within {} seconds",
            timeout.as_secs()
        ),
    )
}

fn stream_idle_error(timeout: Duration) -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Transport,
        format!(
            "OpenAI SSE stream received no data for {} seconds",
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

fn internal(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ProviderErrorKind::Internal, message)
}

#[cfg(test)]
#[path = "openai_tests.rs"]
mod tests;
