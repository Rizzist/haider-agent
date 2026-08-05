//! Provider boundary and deterministic fake provider for the Haider runtime.
//!
//! Owned invariants:
//! - A [`Provider::stream_turn`] stream terminates with a `Finish` event, a
//!   typed [`ProviderError`], or silence-until-drop (`Hang`); nothing follows
//!   an error or a `Finish`.
//! - Text deltas are always complete UTF-8: the fake's [`Utf8Assembler`]
//!   buffers partial scalars, so an invalid partial string never crosses the
//!   trait even when a fixture splits a character across frames.
//! - [`FakeProvider`] is fixture-driven and deterministic — the same script
//!   yields the same event sequence (`Delay` only adds wall time).

mod anthropic;
#[cfg(test)]
mod anthropic_tests;
mod catalog;
#[cfg(test)]
mod catalog_tests;
mod effort;
mod gemini;
#[cfg(test)]
mod gemini_tests;
mod openai;
mod origin;
mod pricing;
mod usage;
mod wire;

use async_trait::async_trait;
use haider_protocol::ids::ArtifactRef;
use haider_protocol::provider::{
    Block, CapabilityDoc, FeatureResolve, FinishReason, StreamEvent, Usage,
};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio::time::{Duration, sleep};

pub use anthropic::{
    ANTHROPIC_API_URL, ANTHROPIC_OAUTH_BASE_URL, ANTHROPIC_OAUTH_BETA_HEADER,
    ANTHROPIC_OAUTH_BETA_VALUE, ANTHROPIC_OAUTH_PROVIDER_NAME, ANTHROPIC_OAUTH_SYSTEM_IDENTITY,
    ANTHROPIC_PROVIDER_NAME, AnthropicCapture, AnthropicProvider, AnthropicRetryPolicy,
    AnthropicTransportConfig, replay_anthropic_http_error, replay_anthropic_sse,
};
pub use catalog::{
    CatalogError, CatalogSource, DiscoveredCatalog, DiscoveredModel, DiscoveredModelExtensions,
    catalog_request_url, discover_models, discover_models_with_resolver,
    openai_compatible_catalog_endpoint, parse_catalog, pickable,
};
pub use effort::{
    anthropic_default_effort, anthropic_fast_mode_supported, anthropic_supported_efforts,
    gemini_default_effort, gemini_supported_efforts,
};
pub use gemini::{
    GEMINI_API_BASE_URL, GEMINI_MODELS_URL, GEMINI_PROVIDER_NAME, GeminiCapture, GeminiProvider,
    GeminiRetryPolicy, GeminiTransportConfig, replay_gemini_http_error, replay_gemini_sse,
    replay_gemini_sse_for_request,
};
pub use openai::{
    KIMI_OAUTH_BASE_URL, KIMI_OAUTH_PROVIDER_NAME, KimiThinkingConfig, KimiThinkingType,
    OPENAI_CODEX_RESPONSES_LITE_HEADER, OPENAI_CODEX_RESPONSES_LITE_VALUE,
    OPENAI_COMPATIBLE_PROVIDER_NAME, OPENAI_OAUTH_PROVIDER_NAME, OPENAI_PROVIDER_NAME,
    OPENAI_RESPONSES_API_URL, OPENAI_SUBSCRIPTION_BASE_URL, OPENAI_SUBSCRIPTION_RESPONSES_URL,
    OpenAiCapture, OpenAiCompatibleProvider, OpenAiProvider, OpenAiRetryPolicy,
    OpenAiTransportConfig, replay_kimi_models_response, replay_openai_chat_sse,
    replay_openai_http_error, replay_openai_models_response, replay_openai_responses_sse,
    validate_openai_compatible_endpoint,
};
pub use origin::{FixedDnsResolver, FixedOriginGuard, SystemFixedDnsResolver};
pub use pricing::{MODEL_RATES, ModelRate, estimate_chunk_cost_usd, model_rate};
pub use usage::{
    ANTHROPIC_OAUTH_USAGE_URL, ANTHROPIC_OAUTH_USAGE_USER_AGENT, KIMI_OAUTH_USAGE_URL,
    MeterReading, MeterUnavailable, OPENAI_OAUTH_USAGE_URL, UsageMeterEndpoint,
    normalize_utilization, parse_anthropic_oauth_usage, parse_kimi_usages, parse_openai_wham_usage,
    parse_rfc3339_to_unix_ms,
};

/// Provider classes backed by production account credentials in this release.
pub const BUILTIN_PROVIDER_NAMES: [&str; 7] = [
    ANTHROPIC_PROVIDER_NAME,
    ANTHROPIC_OAUTH_PROVIDER_NAME,
    OPENAI_PROVIDER_NAME,
    OPENAI_OAUTH_PROVIDER_NAME,
    OPENAI_COMPATIBLE_PROVIDER_NAME,
    KIMI_OAUTH_PROVIDER_NAME,
    GEMINI_PROVIDER_NAME,
];

/// Crate marker used by the workspace self-test.
pub const CRATE_NAME: &str = "haider-provider";

/// One provider-facing conversation message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: MessageRole,
    pub blocks: Vec<Block>,
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            blocks: vec![Block::Text { text: text.into() }],
        }
    }

    pub fn assistant(blocks: Vec<Block>) -> Self {
        Self {
            role: MessageRole::Assistant,
            blocks,
        }
    }

    pub fn tool_result(
        call_id: impl Into<String>,
        preview: impl Into<String>,
        truncated: bool,
    ) -> Self {
        let call_id = call_id.into();
        Self {
            role: MessageRole::Tool,
            blocks: vec![Block::ToolResult {
                call_id,
                preview: preview.into(),
                truncated,
            }],
        }
    }

    pub fn tool_result_for(&self, expected_call_id: &str) -> Option<&Block> {
        (self.role == MessageRole::Tool)
            .then_some(())
            .and_then(|()| {
                self.blocks.iter().find(|block| {
                    matches!(
                        block,
                        Block::ToolResult { call_id, .. } if call_id == expected_call_id
                    )
                })
            })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
    Tool,
}

/// The credential surface an adapter will use for its outbound request.
///
/// This exposes only the authentication class, never credential material. The
/// account factory uses it as an audit pin so an OAuth descriptor cannot be
/// silently routed through an API-key constructor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderCredentialSurface {
    Opaque,
    ApiKey,
    OAuthSubscriptionBearer,
}

/// Provider-local tool definition. The protocol tool manifest has execution
/// and permission fields that do not belong on a model-provider request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// Resolved bytes for one A2 attachment reference.
///
/// The message tree keeps only content-addressed refs. The prompt compiler
/// resolves those refs before crossing the provider boundary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedAttachment {
    pub artifact: ArtifactRef,
    pub data_base64: String,
}

/// Normalized request accepted by every provider adapter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnRequest {
    pub messages: Vec<Message>,
    pub model: String,
    pub max_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDefinition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<ResolvedAttachment>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorKind {
    Authentication,
    PermissionDenied,
    RateLimited,
    Overloaded,
    /// The provider rejected the request because its input does not fit the
    /// active model context window. Core may compact and retry this once.
    ContextExceeded,
    InvalidRequest,
    Transport,
    MalformedFrame,
    InvalidUtf8,
    Internal,
}

/// Typed failure yielded by a provider stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderError {
    pub kind: ProviderErrorKind,
    pub message: String,
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
}

impl ProviderError {
    pub fn new(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            retryable: kind.default_retryable(),
            retry_after_ms: None,
        }
    }

    #[must_use]
    pub fn with_retry_after_ms(mut self, retry_after_ms: Option<u64>) -> Self {
        self.retry_after_ms = retry_after_ms;
        self
    }
}

impl ProviderErrorKind {
    const fn default_retryable(self) -> bool {
        matches!(self, Self::RateLimited | Self::Overloaded | Self::Transport)
    }
}

impl fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for ProviderError {}

pub type ProviderStreamItem = Result<StreamEvent, ProviderError>;

/// Receiver plus ownership of the adapter/script producer task.
///
/// Dropping a turn stream aborts its producer immediately, so cancellation
/// cannot leave an HTTP decoder or fake script detached until an idle timeout.
#[derive(Debug)]
pub struct ProviderStream {
    receiver: mpsc::Receiver<ProviderStreamItem>,
    producer: Option<tokio::task::JoinHandle<()>>,
}

impl ProviderStream {
    pub fn owned(
        receiver: mpsc::Receiver<ProviderStreamItem>,
        producer: tokio::task::JoinHandle<()>,
    ) -> Self {
        Self {
            receiver,
            producer: Some(producer),
        }
    }

    pub async fn recv(&mut self) -> Option<ProviderStreamItem> {
        self.receiver.recv().await
    }
}

impl From<mpsc::Receiver<ProviderStreamItem>> for ProviderStream {
    fn from(receiver: mpsc::Receiver<ProviderStreamItem>) -> Self {
        Self {
            receiver,
            producer: None,
        }
    }
}

impl Drop for ProviderStream {
    fn drop(&mut self) {
        if let Some(producer) = &self.producer {
            producer.abort();
        }
    }
}

/// Asynchronous provider adapter contract.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Describes how this adapter authenticates its outbound request.
    fn credential_surface(&self) -> ProviderCredentialSurface {
        ProviderCredentialSurface::Opaque
    }

    async fn capabilities(&self) -> CapabilityDoc;
    async fn stream_turn(&self, request: TurnRequest) -> Result<ProviderStream, ProviderError>;
}

/// One deterministic operation in a [`FakeProvider`] fixture.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum FakeStep {
    /// Asserts that this request contains the named result from a preceding
    /// request. A `Finish` ends one request segment; the following steps are
    /// consumed only by the next `stream_turn` call.
    ExpectToolResult {
        call_id: String,
    },
    EmitText {
        text: String,
    },
    EmitReasoning {
        text: String,
    },
    /// Emits provider-native continuation state for turn-engine replay tests.
    EmitProviderOpaque {
        provider: String,
        data: serde_json::Value,
    },
    EmitToolCall {
        call_id: String,
        name: String,
        args: serde_json::Value,
    },
    /// Opens a tool call without ending it, for terminal-path fixtures.
    EmitToolCallStart {
        call_id: String,
        name: String,
    },
    /// Streams a partial argument fragment for an open call (no end), for
    /// cancel/error-with-partial-args fixtures.
    EmitToolArgsDelta {
        call_id: String,
        fragment: String,
    },
    /// Emits the canonical `request_input` tool call. The actor, rather than
    /// the fake provider, allocates and journals the protocol menu.
    EmitRequestInput {
        call_id: String,
        kind: FakeInputKind,
        title: String,
        #[serde(default)]
        body: Vec<String>,
        #[serde(default)]
        options: Vec<FakeInputOption>,
    },
    /// Splits the first multibyte scalar after its first byte, then incrementally
    /// decodes both raw chunks. Invalid partial strings never cross the trait.
    SplitUtf8 {
        text: String,
    },
    /// Injects a fixed invalid UTF-8 provider frame.
    MalformedFrame,
    Delay {
        ms: u64,
    },
    EmitUsage {
        usage: Usage,
    },
    Finish {
        reason: FinishReason,
    },
    /// Emits one typed provider error and ends this request segment.
    Error {
        kind: ProviderErrorKind,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_after_ms: Option<u64>,
    },
    /// Produces no more data until the consumer drops the stream.
    Hang,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FakeInputKind {
    Question,
    Choice,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FakeInputOption {
    pub key: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Fixture-driven provider used by runtime and CLI tests.
#[derive(Debug, Clone)]
pub struct FakeProvider {
    script: Arc<Vec<FakeStep>>,
    next_step: Arc<Mutex<usize>>,
    requests: Arc<Mutex<Vec<TurnRequest>>>,
    vision: FeatureResolve,
}

impl FakeProvider {
    pub fn new(script: Vec<FakeStep>) -> Self {
        Self {
            script: Arc::new(script),
            next_step: Arc::new(Mutex::new(0)),
            requests: Arc::new(Mutex::new(Vec::new())),
            vision: FeatureResolve::Unsupported,
        }
    }

    /// Additive fixture switch for tests that need a vision-capable provider.
    #[must_use]
    pub fn with_vision_native(mut self) -> Self {
        self.vision = FeatureResolve::Native;
        self
    }

    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json).map(Self::new)
    }

    /// Requests observed so far, in call order. Poison-tolerant so a panicked
    /// test thread cannot hide the requests it already recorded.
    pub fn requests(&self) -> Vec<TurnRequest> {
        match self.requests.lock() {
            Ok(requests) => requests.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn record_request(&self, request: TurnRequest) {
        match self.requests.lock() {
            Ok(mut requests) => requests.push(request),
            Err(poisoned) => poisoned.into_inner().push(request),
        }
    }
}

#[async_trait]
impl Provider for FakeProvider {
    async fn capabilities(&self) -> CapabilityDoc {
        CapabilityDoc {
            provider: "fake".into(),
            parallel_tools: FeatureResolve::Native,
            streaming_tool_args: FeatureResolve::Native,
            vision: self.vision,
            thinking_visible: FeatureResolve::Native,
            context_limit: 1_000_000,
        }
    }

    async fn stream_turn(&self, request: TurnRequest) -> Result<ProviderStream, ProviderError> {
        self.record_request(request.clone());
        let segment = self.next_segment();
        for step in segment.iter() {
            if let FakeStep::ExpectToolResult { call_id } = step
                && !request
                    .messages
                    .iter()
                    .any(|message| message.tool_result_for(call_id.as_str()).is_some())
            {
                return Err(ProviderError::new(
                    ProviderErrorKind::Internal,
                    format!("expected tool result `{call_id}` in this request"),
                ));
            }
        }
        let (sender, receiver) = mpsc::channel(32);
        let producer = tokio::spawn(play_script(segment, sender));
        Ok(ProviderStream::owned(receiver, producer))
    }
}

impl FakeProvider {
    fn next_segment(&self) -> Arc<Vec<FakeStep>> {
        let mut next = self
            .next_step
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let start = *next;
        let mut end = start;
        while end < self.script.len() {
            end += 1;
            if matches!(
                self.script[end - 1],
                FakeStep::Finish { .. }
                    | FakeStep::Error { .. }
                    | FakeStep::Hang
                    | FakeStep::MalformedFrame
            ) {
                break;
            }
        }
        *next = end;
        Arc::new(self.script[start..end].to_vec())
    }
}

/// Plays one fixture script into `sender`. Stops early once the consumer
/// drops the stream; otherwise ends with `Finish`, a typed error, or (for a
/// script that ends mid-scalar) a trailing invalid-UTF-8 error.
async fn play_script(script: Arc<Vec<FakeStep>>, sender: mpsc::Sender<ProviderStreamItem>) {
    let mut utf8 = Utf8Assembler::default();
    for step in script.iter().cloned() {
        match step {
            FakeStep::ExpectToolResult { .. } => {}
            FakeStep::EmitText { text } => {
                if !emit_bytes(&sender, &mut utf8, text.as_bytes()).await {
                    return;
                }
            }
            FakeStep::EmitReasoning { text } => {
                if !send_event(&sender, StreamEvent::ReasoningDelta { text }).await {
                    return;
                }
            }
            FakeStep::EmitProviderOpaque { provider, data } => {
                if !send_event(&sender, StreamEvent::ProviderOpaque { provider, data }).await {
                    return;
                }
            }
            FakeStep::EmitToolCall {
                call_id,
                name,
                args,
            } => {
                if !emit_tool_call(&sender, call_id, name, args).await {
                    return;
                }
            }
            FakeStep::EmitToolCallStart { call_id, name } => {
                if !send_event(&sender, StreamEvent::ToolCallStart { call_id, name }).await {
                    return;
                }
            }
            FakeStep::EmitToolArgsDelta { call_id, fragment } => {
                if !send_event(
                    &sender,
                    StreamEvent::ToolCallArgsDelta {
                        call_id,
                        args_fragment: fragment,
                    },
                )
                .await
                {
                    return;
                }
            }
            FakeStep::EmitRequestInput {
                call_id,
                kind,
                title,
                body,
                options,
            } => {
                let args = serde_json::json!({
                    "kind": match kind {
                        FakeInputKind::Question => "question",
                        FakeInputKind::Choice => "choice",
                    },
                    "title": title,
                    "body": body,
                    "options": options,
                });
                if !emit_tool_call(&sender, call_id, "request_input".into(), args).await {
                    return;
                }
            }
            FakeStep::SplitUtf8 { text } => {
                let Some(split) = split_inside_multibyte(&text) else {
                    let _ = sender
                        .send(Err(ProviderError::new(
                            ProviderErrorKind::InvalidUtf8,
                            "split_utf8 requires at least one multibyte character",
                        )))
                        .await;
                    return;
                };
                let bytes = text.as_bytes();
                if !emit_bytes(&sender, &mut utf8, &bytes[..split]).await
                    || !emit_bytes(&sender, &mut utf8, &bytes[split..]).await
                {
                    return;
                }
            }
            FakeStep::MalformedFrame => {
                // The fixed bytes are invalid UTF-8, so the assembler always
                // turns this into a typed MalformedFrame stream error.
                let _ = emit_bytes(&sender, &mut utf8, &[0xf0, 0x28, 0x8c, 0x28]).await;
                return;
            }
            FakeStep::Delay { ms } => sleep(Duration::from_millis(ms)).await,
            FakeStep::EmitUsage { usage } => {
                if !send_event(&sender, StreamEvent::UsageUpdate(usage)).await {
                    return;
                }
            }
            FakeStep::Finish { reason } => {
                let _ = send_event(&sender, StreamEvent::Finish { reason }).await;
                return;
            }
            FakeStep::Error {
                kind,
                message,
                retry_after_ms,
            } => {
                let _ = sender
                    .send(Err(
                        ProviderError::new(kind, message).with_retry_after_ms(retry_after_ms)
                    ))
                    .await;
                return;
            }
            FakeStep::Hang => {
                sender.closed().await;
                return;
            }
        }
    }

    if utf8.has_pending() {
        let _ = sender
            .send(Err(ProviderError::new(
                ProviderErrorKind::InvalidUtf8,
                "provider stream ended inside a UTF-8 scalar",
            )))
            .await;
    }
}

/// Emits start → full-args delta → end for one scripted tool call.
/// Returns false once the stream should stop (consumer gone or error sent).
async fn emit_tool_call(
    sender: &mpsc::Sender<ProviderStreamItem>,
    call_id: String,
    name: String,
    args: serde_json::Value,
) -> bool {
    if !send_event(
        sender,
        StreamEvent::ToolCallStart {
            call_id: call_id.clone(),
            name,
        },
    )
    .await
    {
        return false;
    }
    let args_fragment = match serde_json::to_string(&args) {
        Ok(fragment) => fragment,
        Err(error) => {
            let _ = sender
                .send(Err(ProviderError::new(
                    ProviderErrorKind::Internal,
                    format!("fake tool arguments could not serialize: {error}"),
                )))
                .await;
            return false;
        }
    };
    send_event(
        sender,
        StreamEvent::ToolCallArgsDelta {
            call_id: call_id.clone(),
            args_fragment,
        },
    )
    .await
        && send_event(sender, StreamEvent::ToolCallEnd { call_id }).await
}

/// Returns false when the consumer has dropped the stream.
async fn send_event(sender: &mpsc::Sender<ProviderStreamItem>, event: StreamEvent) -> bool {
    sender.send(Ok(event)).await.is_ok()
}

/// Decodes raw bytes through the assembler and emits every complete scalar
/// run as a text delta. Returns false once the stream should stop (consumer
/// gone or decode error already sent).
async fn emit_bytes(
    sender: &mpsc::Sender<ProviderStreamItem>,
    utf8: &mut Utf8Assembler,
    bytes: &[u8],
) -> bool {
    match utf8.push(bytes) {
        Ok(parts) => {
            for text in parts {
                if !send_event(sender, StreamEvent::TextDelta { text }).await {
                    return false;
                }
            }
            true
        }
        Err(error) => {
            let _ = sender.send(Err(error)).await;
            false
        }
    }
}

/// Byte index one past the start of the first multibyte character — i.e. a
/// split point guaranteed to fall inside that character's encoding.
fn split_inside_multibyte(text: &str) -> Option<usize> {
    text.char_indices()
        .find(|(_, character)| character.len_utf8() > 1)
        .map(|(index, _)| index + 1)
}

/// Incremental UTF-8 decoder: buffers a trailing partial scalar between
/// pushes so only complete, valid text ever leaves the fake provider.
#[derive(Debug, Default)]
pub(crate) struct Utf8Assembler {
    pending: Vec<u8>,
}

impl Utf8Assembler {
    /// Returns the complete text now decodable, buffering any trailing
    /// partial scalar; an invalid (not merely incomplete) sequence is an error.
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, ProviderError> {
        self.pending.extend_from_slice(bytes);
        let mut decoded = Vec::new();

        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(text) => {
                    if !text.is_empty() {
                        decoded.push(text.to_owned());
                    }
                    self.pending.clear();
                    return Ok(decoded);
                }
                Err(error) if error.error_len().is_some() => {
                    self.pending.clear();
                    return Err(ProviderError::new(
                        ProviderErrorKind::MalformedFrame,
                        format!(
                            "provider frame contains invalid UTF-8 at byte {}",
                            error.valid_up_to()
                        ),
                    ));
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    if valid == 0 {
                        return Ok(decoded);
                    }
                    let prefix = String::from_utf8(self.pending.drain(..valid).collect()).map_err(
                        |conversion| {
                            ProviderError::new(
                                ProviderErrorKind::Internal,
                                format!("validated UTF-8 prefix failed conversion: {conversion}"),
                            )
                        },
                    )?;
                    decoded.push(prefix);
                }
            }
        }
    }

    pub(crate) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}
