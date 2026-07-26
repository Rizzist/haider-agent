//! Anthropic Messages API adapter.

use std::time::SystemTime;

use async_trait::async_trait;
use haider_accounts::SecretHandle;
use haider_protocol::ids::CredentialAlias;
use haider_protocol::provider::{CapabilityDoc, FeatureResolve};
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderValue, RETRY_AFTER};
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::wire::{SseDecoder, WireApiError, provider_kind_name, request_json};
use crate::{
    Provider, ProviderError, ProviderErrorKind, ProviderStream, ProviderStreamItem, TurnRequest,
};

pub const ANTHROPIC_PROVIDER_NAME: &str = "anthropic";
pub const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const STREAM_CAPACITY: usize = 32;

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
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| {
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
        })
    }

    #[must_use]
    pub fn with_account(mut self, account: CredentialAlias) -> Self {
        self.account = Some(account);
        self
    }

    /// Overrides the endpoint for an explicit capture/test harness.
    #[must_use]
    pub fn with_api_url(mut self, api_url: impl Into<String>) -> Self {
        self.api_url = api_url.into();
        self
    }

    /// Builds the secret-free JSON body. Capture tools use this to record the
    /// exact payload shape without gaining access to the credential.
    pub fn request_payload(
        &self,
        request: &TurnRequest,
    ) -> Result<serde_json::Value, ProviderError> {
        self.validate_model(request)?;
        request_json(request)
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

    async fn send_request(
        &self,
        request: &TurnRequest,
    ) -> Result<reqwest::Response, ProviderError> {
        let payload = self.request_payload(request)?;
        self.client
            .post(&self.api_url)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "text/event-stream")
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("x-api-key", self.api_key_header()?)
            .json(&payload)
            .send()
            .await
            .map_err(transport_error)
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
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
        tokio::spawn(async move {
            stream_response(response, account, sender).await;
        });
        Ok(receiver)
    }
}

#[derive(Debug, Clone, Copy)]
struct ModelCapabilities {
    context_limit: u64,
    thinking_visible: FeatureResolve,
}

fn model_capabilities(model: &str) -> ModelCapabilities {
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
    mut response: reqwest::Response,
    account: Option<CredentialAlias>,
    sender: mpsc::Sender<ProviderStreamItem>,
) {
    let mut decoder = SseDecoder::new(account);
    loop {
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => {
                send_items(&sender, decoder.finish()).await;
                return;
            }
            Err(error) => {
                let _ = sender.send(Err(transport_error(error))).await;
                return;
            }
        };
        if !send_items(&sender, decoder.push(&chunk)).await || decoder.is_terminal() {
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
    let kind = match status {
        401 => ProviderErrorKind::Authentication,
        403 => ProviderErrorKind::PermissionDenied,
        429 => ProviderErrorKind::RateLimited,
        529 => ProviderErrorKind::Overloaded,
        500..=599 => ProviderErrorKind::Transport,
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
