//! Anthropic Messages wire types and the incremental SSE state machine.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use haider_protocol::ids::CredentialAlias;
use haider_protocol::provider::{Block, FinishReason, StreamEvent, Usage, UsageSource};
use haider_protocol::tool::AttachmentBlock;
use serde::Deserialize;

use crate::{
    MessageRole, ProviderError, ProviderErrorKind, ProviderStreamItem, TurnRequest, Utf8Assembler,
};

/// The exact identity line Anthropic's Messages API requires at the head of
/// the `system` field on OAuth-subscription (Pro/Max bearer-token) requests.
///
/// The server silently validates OAuth-authenticated request bodies: unless
/// the system prompt opens with this line — as its own first block when
/// `system` is an array — the request is rejected with a deliberately generic
/// HTTP 400 `invalid_request_error` whose message is just "Error". Omitting
/// `system` entirely is rejected the same way, and concatenating the identity
/// into a larger first block does not pass. The check is undocumented,
/// server-side, and does not apply to `x-api-key` requests, so it is modeled
/// here as an OAuth-only body shape rather than a change to the shared prompt.
pub const ANTHROPIC_OAUTH_SYSTEM_IDENTITY: &str =
    "You are Claude Code, Anthropic's official CLI for Claude.";

/// How the `system` field of the Anthropic Messages body must be shaped for
/// the active authentication mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AnthropicSystemShape {
    /// `x-api-key` mode: the turn's system prompt rides as a plain string and
    /// the field is omitted when the turn has none.
    ApiKey,
    /// OAuth-subscription bearer mode: `system` is always an array whose
    /// first block is exactly [`ANTHROPIC_OAUTH_SYSTEM_IDENTITY`]; the turn's
    /// real system prompt follows as its own second block.
    OAuthClaudeCode,
}

pub(crate) fn request_json(
    request: &TurnRequest,
    system_shape: AnthropicSystemShape,
    effort: Option<&str>,
    fast: bool,
) -> Result<serde_json::Value, ProviderError> {
    let attachments = attachment_index(request)?;
    let messages = request
        .messages
        .iter()
        .map(|message| {
            let role = match message.role {
                MessageRole::User | MessageRole::Tool => "user",
                MessageRole::Assistant => "assistant",
            };
            let content = message
                .blocks
                .iter()
                .map(|block| content_block(message.role, block, &attachments))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(serde_json::json!({
                "role": role,
                "content": content,
            }))
        })
        .collect::<Result<Vec<_>, ProviderError>>()?;
    let tools = request
        .tools
        .iter()
        .map(|tool| {
            serde_json::json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": anthropic_tool_schema(&tool.input_schema),
            })
        })
        .collect::<Vec<_>>();
    let mut payload = serde_json::json!({
        "model": request.model,
        "max_tokens": request.max_tokens,
        "messages": messages,
        "stream": true,
    });
    let Some(object) = payload.as_object_mut() else {
        return Err(ProviderError::new(
            ProviderErrorKind::Internal,
            "Anthropic request payload was not a JSON object",
        ));
    };
    match system_shape {
        AnthropicSystemShape::ApiKey => {
            if let Some(system) = &request.system_prompt {
                object.insert("system".into(), serde_json::Value::String(system.clone()));
            }
        }
        AnthropicSystemShape::OAuthClaudeCode => {
            let mut blocks = vec![serde_json::json!({
                "type": "text",
                "text": ANTHROPIC_OAUTH_SYSTEM_IDENTITY,
            })];
            if let Some(system) = &request.system_prompt {
                blocks.push(serde_json::json!({"type": "text", "text": system}));
            }
            object.insert("system".into(), serde_json::Value::Array(blocks));
        }
    }
    if !tools.is_empty() {
        object.insert("tools".into(), serde_json::Value::Array(tools));
    }
    // G3 effort rides `output_config.effort` VERBATIM when set (GA, no beta
    // header). NEVER `thinking.budget_tokens` — that shape 400s on 4.7+ and
    // every 5-family model — and no `thinking` field is emitted otherwise.
    if let Some(effort) = effort {
        object.insert(
            "output_config".into(),
            serde_json::json!({"effort": effort}),
        );
    }
    // G3 fast mode: top-level `speed: "fast"`; the paired
    // `fast-mode-2026-02-01` beta header is applied by the HTTP adapter.
    if fast {
        object.insert("speed".into(), serde_json::json!("fast"));
    }
    Ok(payload)
}

/// Shapes one custom tool `input_schema` for Anthropic's Messages API, which
/// rejects `oneOf`/`allOf`/`anyOf` at the top level of a tool schema with
/// HTTP 400 `invalid_request_error` ("input_schema does not support oneOf,
/// allOf, or anyOf at the top level"). The constraint is auth-agnostic — it
/// fires for `x-api-key` and OAuth requests alike — so exactly those three
/// top-level keys are dropped for every Anthropic request. Nested combinators
/// are untouched, and the dropped clauses stay enforced where they always
/// were: the tool executor validates arguments before running.
fn anthropic_tool_schema(schema: &serde_json::Value) -> serde_json::Value {
    let mut schema = schema.clone();
    if let Some(object) = schema.as_object_mut() {
        object.remove("oneOf");
        object.remove("allOf");
        object.remove("anyOf");
    }
    schema
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

fn content_block(
    role: MessageRole,
    block: &Block,
    attachments: &HashMap<&str, &str>,
) -> Result<serde_json::Value, ProviderError> {
    match block {
        Block::Text { text } => Ok(serde_json::json!({"type": "text", "text": text})),
        Block::Reasoning { .. } => Err(invalid_request(
            "normalized reasoning summaries cannot be replayed as signed Anthropic thinking blocks",
        )),
        Block::ToolCall {
            call_id,
            name,
            args,
        } if role == MessageRole::Assistant => Ok(serde_json::json!({
            "type": "tool_use",
            "id": call_id,
            "name": name,
            "input": args,
        })),
        Block::ToolCall { .. } => Err(invalid_request(
            "Anthropic tool_use blocks are only valid in assistant messages",
        )),
        Block::ToolResult {
            call_id, preview, ..
        } if matches!(role, MessageRole::User | MessageRole::Tool) => Ok(serde_json::json!({
            "type": "tool_result",
            "tool_use_id": call_id,
            "content": preview,
        })),
        Block::ToolResult { .. } => Err(invalid_request(
            "Anthropic tool_result blocks are only valid in user/tool messages",
        )),
        Block::Attachment(AttachmentBlock::Image { artifact, mime, .. })
            if role == MessageRole::User =>
        {
            let data = attachments.get(artifact.as_str()).ok_or_else(|| {
                invalid_request(format!(
                    "image attachment `{artifact}` has no resolved base64 data"
                ))
            })?;
            if !matches!(
                mime.as_str(),
                "image/jpeg" | "image/png" | "image/gif" | "image/webp"
            ) {
                return Err(invalid_request(format!(
                    "image attachment `{artifact}` has unsupported MIME type `{mime}`"
                )));
            }
            Ok(serde_json::json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": mime,
                    "data": data,
                }
            }))
        }
        Block::Attachment(AttachmentBlock::Image { .. }) => Err(invalid_request(
            "Anthropic image blocks are only valid in user messages",
        )),
        Block::Attachment(AttachmentBlock::PastedText { artifact, .. }) => Err(invalid_request(
            format!("pasted-text attachment `{artifact}` was not resolved by the prompt compiler"),
        )),
        Block::Attachment(AttachmentBlock::Skill { name, .. }) => Err(invalid_request(format!(
            "skill attachment `{name}` was not resolved by the prompt compiler"
        ))),
        Block::ProviderOpaque { provider, data } if provider == "anthropic" && data.is_object() => {
            Ok(data.clone())
        }
        Block::ProviderOpaque { provider, .. } if provider == "anthropic" => Err(invalid_request(
            "Anthropic provider-opaque content block must be a JSON object",
        )),
        Block::ProviderOpaque { provider, .. } => Err(invalid_request(format!(
            "provider-opaque block for `{provider}` cannot be sent to Anthropic"
        ))),
    }
}

fn invalid_request(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ProviderErrorKind::InvalidRequest, message)
}

#[derive(Debug)]
pub(crate) struct SseDecoder {
    utf8: Utf8Assembler,
    line_buffer: String,
    event_name: Option<String>,
    data_lines: Vec<String>,
    state: StreamState,
    terminal: bool,
}

impl SseDecoder {
    pub(crate) fn new(account: Option<CredentialAlias>) -> Self {
        Self {
            utf8: Utf8Assembler::default(),
            line_buffer: String::new(),
            event_name: None,
            data_lines: Vec::new(),
            state: StreamState::new(account),
            terminal: false,
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<ProviderStreamItem> {
        if self.terminal {
            return Vec::new();
        }
        let decoded = match self.utf8.push(bytes) {
            Ok(decoded) => decoded,
            Err(error) => return self.fail(error),
        };
        let mut output = Vec::new();
        for text in decoded {
            self.line_buffer.push_str(&text);
            while let Some(newline) = self.line_buffer.find('\n') {
                let mut line = self.line_buffer.drain(..=newline).collect::<String>();
                line.pop();
                if line.ends_with('\r') {
                    line.pop();
                }
                if let Some(item) = self.accept_line(&line) {
                    let terminal = matches!(item, Err(_) | Ok(StreamEvent::Finish { .. }));
                    output.push(item);
                    if terminal {
                        self.terminal = true;
                        return output;
                    }
                }
            }
        }
        output
    }

    pub(crate) fn finish(&mut self) -> Vec<ProviderStreamItem> {
        if self.terminal {
            return Vec::new();
        }
        if self.utf8.has_pending() {
            return self.fail(malformed(
                "Anthropic SSE stream ended inside a UTF-8 scalar",
            ));
        }
        if !self.line_buffer.is_empty() {
            let line = std::mem::take(&mut self.line_buffer);
            if let Some(item) = self.accept_line(line.trim_end_matches('\r')) {
                let terminal = matches!(item, Err(_) | Ok(StreamEvent::Finish { .. }));
                self.terminal = terminal;
                return vec![item];
            }
        }
        if (self.event_name.is_some() || !self.data_lines.is_empty())
            && let Some(item) = self.dispatch_event()
        {
            let terminal = matches!(item, Err(_) | Ok(StreamEvent::Finish { .. }));
            self.terminal = terminal;
            return vec![item];
        }
        self.fail(malformed(
            "Anthropic SSE stream ended before a message_stop event",
        ))
    }

    pub(crate) const fn is_terminal(&self) -> bool {
        self.terminal
    }

    fn accept_line(&mut self, line: &str) -> Option<ProviderStreamItem> {
        if line.is_empty() {
            return self.dispatch_event();
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

    fn dispatch_event(&mut self) -> Option<ProviderStreamItem> {
        let event_name = self.event_name.take();
        if self.data_lines.is_empty() {
            return None;
        }
        let data = self.data_lines.join("\n");
        self.data_lines.clear();
        let Some(event_name) = event_name else {
            return Some(Err(malformed("Anthropic SSE data frame has no event name")));
        };
        let value: serde_json::Value = match serde_json::from_str(&data) {
            Ok(value) => value,
            Err(error) => {
                return Some(Err(malformed(format!(
                    "Anthropic SSE `{event_name}` data is not valid JSON: {error}"
                ))));
            }
        };
        let data_type = value.get("type").and_then(serde_json::Value::as_str);
        if known_event_name(&event_name) && data_type != Some(event_name.as_str()) {
            return Some(Err(malformed(format!(
                "Anthropic SSE event `{event_name}` disagrees with data type `{}`",
                data_type.unwrap_or("<missing>")
            ))));
        }
        if !known_event_name(&event_name) {
            return None;
        }
        let event = match serde_json::from_value(value) {
            Ok(event) => event,
            Err(error) => {
                return Some(Err(malformed(format!(
                    "Anthropic SSE `{event_name}` frame has an invalid shape: {error}"
                ))));
            }
        };
        self.state.apply(event).transpose()
    }

    fn fail(&mut self, error: ProviderError) -> Vec<ProviderStreamItem> {
        self.terminal = true;
        vec![Err(error)]
    }
}

fn known_event_name(name: &str) -> bool {
    matches!(
        name,
        "message_start"
            | "content_block_start"
            | "content_block_delta"
            | "content_block_stop"
            | "message_delta"
            | "message_stop"
            | "ping"
            | "error"
    )
}

#[derive(Debug)]
struct StreamState {
    account: Option<CredentialAlias>,
    started: bool,
    open_blocks: BTreeMap<usize, OpenBlock>,
    seen_blocks: BTreeSet<usize>,
    message_delta_seen: bool,
    input: InputUsage,
    stop_reason: Option<FinishReason>,
}

impl StreamState {
    fn new(account: Option<CredentialAlias>) -> Self {
        Self {
            account,
            started: false,
            open_blocks: BTreeMap::new(),
            seen_blocks: BTreeSet::new(),
            message_delta_seen: false,
            input: InputUsage::default(),
            stop_reason: None,
        }
    }

    fn apply(&mut self, event: WireEvent) -> Result<Option<StreamEvent>, ProviderError> {
        match event {
            WireEvent::MessageStart { message } => {
                if self.started {
                    return Err(malformed("received a second Anthropic message_start"));
                }
                if !message.content.is_empty() {
                    return Err(malformed(
                        "Anthropic message_start contained non-empty content",
                    ));
                }
                self.started = true;
                self.input.update(&message.usage)?;
                Ok(None)
            }
            WireEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                self.require_started("content_block_start")?;
                self.require_before_message_delta("content_block_start")?;
                if !self.seen_blocks.insert(index) {
                    return Err(malformed(format!(
                        "Anthropic content block index {index} started twice"
                    )));
                }
                let (block, item) = match content_block {
                    WireContentBlock::Text { text } => (
                        OpenBlock::Text,
                        (!text.is_empty()).then_some(StreamEvent::TextDelta { text }),
                    ),
                    WireContentBlock::ToolUse { id, name, input } => {
                        if !input.as_object().is_some_and(serde_json::Map::is_empty) {
                            return Err(malformed(format!(
                                "Anthropic tool block index {index} started with non-empty input"
                            )));
                        }
                        (
                            OpenBlock::Tool {
                                call_id: id.clone(),
                            },
                            Some(StreamEvent::ToolCallStart { call_id: id, name }),
                        )
                    }
                    WireContentBlock::Thinking { .. } => (OpenBlock::Thinking, None),
                    WireContentBlock::RedactedThinking
                    | WireContentBlock::Fallback
                    | WireContentBlock::Unknown => (OpenBlock::Opaque, None),
                };
                self.open_blocks.insert(index, block);
                Ok(item)
            }
            WireEvent::ContentBlockDelta { index, delta } => {
                self.require_started("content_block_delta")?;
                self.require_before_message_delta("content_block_delta")?;
                let block = self.open_blocks.get(&index).ok_or_else(|| {
                    malformed(format!(
                        "Anthropic delta references unopened content block index {index}"
                    ))
                })?;
                match (block, delta) {
                    (OpenBlock::Text, WireDelta::Text { text }) => {
                        Ok(Some(StreamEvent::TextDelta { text }))
                    }
                    (OpenBlock::Tool { call_id }, WireDelta::InputJson { partial_json }) => {
                        Ok(Some(StreamEvent::ToolCallArgsDelta {
                            call_id: call_id.clone(),
                            args_fragment: partial_json,
                        }))
                    }
                    (OpenBlock::Thinking, WireDelta::Thinking { thinking }) => {
                        Ok(Some(StreamEvent::ReasoningDelta { text: thinking }))
                    }
                    (OpenBlock::Thinking, WireDelta::Signature) | (_, WireDelta::Unknown) => {
                        Ok(None)
                    }
                    _ => Err(malformed(format!(
                        "Anthropic delta type does not match content block index {index}"
                    ))),
                }
            }
            WireEvent::ContentBlockStop { index } => {
                self.require_started("content_block_stop")?;
                self.require_before_message_delta("content_block_stop")?;
                match self.open_blocks.remove(&index) {
                    Some(OpenBlock::Tool { call_id }) => {
                        Ok(Some(StreamEvent::ToolCallEnd { call_id }))
                    }
                    Some(OpenBlock::Text | OpenBlock::Thinking | OpenBlock::Opaque) => Ok(None),
                    None => Err(malformed(format!(
                        "Anthropic stop references unopened content block index {index}"
                    ))),
                }
            }
            WireEvent::MessageDelta { delta, usage } => {
                self.require_started("message_delta")?;
                if !self.open_blocks.is_empty() {
                    return Err(malformed(
                        "Anthropic message_delta arrived while a content block was open",
                    ));
                }
                if let Some(stop_reason) = delta.stop_reason {
                    let normalized = normalize_stop_reason(&stop_reason)?;
                    if self
                        .stop_reason
                        .is_some_and(|existing| existing != normalized)
                    {
                        return Err(malformed(
                            "Anthropic message_delta changed an existing stop_reason",
                        ));
                    }
                    self.stop_reason = Some(normalized);
                }
                self.message_delta_seen = true;
                let Some(usage) = usage else {
                    return Ok(None);
                };
                self.input.update(&usage)?;
                let input = self
                    .input
                    .input_tokens
                    .checked_add(self.input.cache_creation_input_tokens)
                    .ok_or_else(|| malformed("Anthropic input usage counter overflowed u64"))?;
                Ok(Some(StreamEvent::UsageUpdate(Usage {
                    input,
                    output: usage.output_tokens.unwrap_or(0),
                    reasoning: 0,
                    cached: self.input.cache_read_input_tokens,
                    source: UsageSource::ProviderReported,
                    account: self.account.clone(),
                    accounts: Vec::new(),
                })))
            }
            WireEvent::MessageStop => {
                self.require_started("message_stop")?;
                if !self.open_blocks.is_empty() {
                    return Err(malformed(
                        "Anthropic message_stop arrived while a content block was open",
                    ));
                }
                let reason = self.stop_reason.ok_or_else(|| {
                    malformed("Anthropic message_stop arrived without a stop_reason")
                })?;
                Ok(Some(StreamEvent::Finish { reason }))
            }
            WireEvent::Ping | WireEvent::Unknown => Ok(None),
            WireEvent::Error { error } => Err(api_error(error)),
        }
    }

    fn require_started(&self, event: &str) -> Result<(), ProviderError> {
        if self.started {
            Ok(())
        } else {
            Err(malformed(format!(
                "Anthropic {event} arrived before message_start"
            )))
        }
    }

    fn require_before_message_delta(&self, event: &str) -> Result<(), ProviderError> {
        if self.message_delta_seen {
            Err(malformed(format!(
                "Anthropic {event} arrived after message_delta"
            )))
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Default)]
struct InputUsage {
    input_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_read_input_tokens: u64,
}

impl InputUsage {
    fn update(&mut self, usage: &WireUsage) -> Result<(), ProviderError> {
        if let Some(value) = usage.input_tokens {
            self.input_tokens = value;
        }
        if let Some(value) = usage.cache_creation_input_tokens {
            self.cache_creation_input_tokens = value;
        }
        if let Some(value) = usage.cache_read_input_tokens {
            self.cache_read_input_tokens = value;
        }
        self.input_tokens
            .checked_add(self.cache_creation_input_tokens)
            .ok_or_else(|| malformed("Anthropic input usage counter overflowed u64"))?;
        Ok(())
    }
}

#[derive(Debug)]
enum OpenBlock {
    Text,
    Tool { call_id: String },
    Thinking,
    Opaque,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireEvent {
    MessageStart {
        message: WireMessage,
    },
    ContentBlockStart {
        index: usize,
        content_block: WireContentBlock,
    },
    ContentBlockDelta {
        index: usize,
        delta: WireDelta,
    },
    ContentBlockStop {
        index: usize,
    },
    MessageDelta {
        delta: WireMessageDelta,
        #[serde(default)]
        usage: Option<WireUsage>,
    },
    MessageStop,
    Ping,
    Error {
        error: WireApiError,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct WireMessage {
    #[serde(default)]
    content: Vec<serde_json::Value>,
    #[serde(default)]
    usage: WireUsage,
}

#[derive(Debug, Default, Deserialize)]
struct WireUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireContentBlock {
    Text {
        #[serde(default)]
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    Thinking {
        #[serde(default, rename = "thinking")]
        _thinking: String,
    },
    RedactedThinking,
    Fallback,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireDelta {
    #[serde(rename = "text_delta")]
    Text { text: String },
    #[serde(rename = "input_json_delta")]
    InputJson { partial_json: String },
    #[serde(rename = "thinking_delta")]
    Thinking { thinking: String },
    #[serde(rename = "signature_delta")]
    Signature,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Default, Deserialize)]
struct WireMessageDelta {
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WireApiError {
    #[serde(rename = "type")]
    pub(crate) kind: String,
    #[serde(rename = "message")]
    pub(crate) message: String,
}

pub(crate) fn api_error(error: WireApiError) -> ProviderError {
    let kind = if is_anthropic_context_error(&error.kind, &error.message) {
        ProviderErrorKind::ContextExceeded
    } else {
        match error.kind.as_str() {
            "authentication_error" => ProviderErrorKind::Authentication,
            "permission_error" => ProviderErrorKind::PermissionDenied,
            "rate_limit_error" => ProviderErrorKind::RateLimited,
            "overloaded_error" => ProviderErrorKind::Overloaded,
            "api_error" | "timeout_error" => ProviderErrorKind::Transport,
            _ => ProviderErrorKind::InvalidRequest,
        }
    };
    ProviderError::new(
        kind,
        format!("Anthropic API returned {}", provider_kind_name(kind)),
    )
}

pub(crate) const fn provider_kind_name(kind: ProviderErrorKind) -> &'static str {
    match kind {
        ProviderErrorKind::Authentication => "an authentication error",
        ProviderErrorKind::PermissionDenied => "a permission error",
        ProviderErrorKind::RateLimited => "a rate-limit error",
        ProviderErrorKind::Overloaded => "an overloaded error",
        ProviderErrorKind::ContextExceeded => "a context-window-exceeded error",
        ProviderErrorKind::InvalidRequest => "an invalid-request error",
        ProviderErrorKind::Transport => "a transport error",
        ProviderErrorKind::MalformedFrame => "a malformed-frame error",
        ProviderErrorKind::InvalidUtf8 => "an invalid-UTF-8 error",
        ProviderErrorKind::Internal => "an internal adapter error",
    }
}

fn normalize_stop_reason(reason: &str) -> Result<FinishReason, ProviderError> {
    match reason {
        "end_turn" | "stop_sequence" | "pause_turn" => Ok(FinishReason::EndTurn),
        "tool_use" => Ok(FinishReason::ToolUse),
        "max_tokens" => Ok(FinishReason::MaxTokens),
        "model_context_window_exceeded" => Err(ProviderError::new(
            ProviderErrorKind::ContextExceeded,
            "Anthropic reported model_context_window_exceeded",
        )),
        "refusal" => Ok(FinishReason::Refusal),
        _ => Err(malformed(format!(
            "Anthropic returned unknown stop_reason `{reason}`"
        ))),
    }
}

pub(crate) fn is_anthropic_context_error(kind: &str, message: &str) -> bool {
    matches!(
        kind,
        "context_window_exceeded" | "model_context_window_exceeded" | "prompt_too_long"
    ) || (kind == "invalid_request_error"
        && [
            "context window",
            "prompt is too long",
            "input is too long",
            "too many tokens",
        ]
        .iter()
        .any(|needle| message.to_ascii_lowercase().contains(needle)))
}

fn malformed(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ProviderErrorKind::MalformedFrame, message)
}
