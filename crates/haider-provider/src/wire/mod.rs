//! Anthropic Messages wire types and the incremental SSE state machine.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use haider_protocol::ids::CredentialAlias;
use haider_protocol::provider::{
    Block, CacheStatAvailability, FinishReason, NormalizedUsage, ReasoningAccounting, StreamEvent,
    Usage, UsageSource, WebSource,
};
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
    web_tools: bool,
    cache_ttl: Option<crate::AnthropicCacheTtl>,
) -> Result<serde_json::Value, ProviderError> {
    let attachments = attachment_index(request)?;
    let mut messages = request
        .messages
        .iter()
        .map(|message| {
            let role = match message.role {
                MessageRole::User | MessageRole::Tool => "user",
                MessageRole::Assistant => "assistant",
            };
            let content = message_content(message.role, &message.blocks, &attachments)?;
            Ok(serde_json::json!({
                "role": role,
                "content": content,
            }))
        })
        .collect::<Result<Vec<_>, ProviderError>>()?;
    let mut tools = request
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
    // W-B (LW1): Anthropic SERVER web tools ride the same `tools` array as
    // typed entries with these EXACT shapes — `web_search_20250305` (the
    // basic, all-models version; deliberately NOT the 2026 dynamic-filtering
    // versions) and `web_fetch_20250910` with citations disabled. The daemon
    // gates the flag per resolved pair; enterprise (Bedrock/Vertex) and
    // non-Anthropic pairs never set it.
    if web_tools {
        tools.push(serde_json::json!({
            "type": "web_search_20250305",
            "name": "web_search",
            "max_uses": 8,
        }));
        tools.push(serde_json::json!({
            "type": "web_fetch_20250910",
            "name": "web_fetch",
            "citations": {"enabled": false},
            "max_content_tokens": 100000,
            "max_uses": 10,
        }));
    }
    if let Some(ttl) = cache_ttl {
        if let Some(tool) = tools.last_mut().and_then(serde_json::Value::as_object_mut) {
            tool.insert("cache_control".into(), anthropic_cache_control(ttl));
        }
        if let Some(metadata) = &request.cache_metadata {
            if let Some(boundary) = metadata.latest_compaction_summary_end {
                annotate_anthropic_message_boundary(
                    &mut messages,
                    &request.messages,
                    boundary,
                    ttl,
                );
            }
            annotate_anthropic_message_boundary(
                &mut messages,
                &request.messages,
                metadata.stable_history_end,
                ttl,
            );
        }
    }
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
                if let Some(ttl) = cache_ttl {
                    object.insert(
                        "system".into(),
                        serde_json::json!([{
                            "type": "text",
                            "text": system,
                            "cache_control": anthropic_cache_control(ttl),
                        }]),
                    );
                } else {
                    object.insert("system".into(), serde_json::Value::String(system.clone()));
                }
            }
        }
        AnthropicSystemShape::OAuthClaudeCode => {
            let mut blocks = vec![serde_json::json!({
                "type": "text",
                "text": ANTHROPIC_OAUTH_SYSTEM_IDENTITY,
            })];
            if let Some(system) = &request.system_prompt {
                let mut block = serde_json::json!({"type": "text", "text": system});
                if let Some(ttl) = cache_ttl
                    && let Some(object) = block.as_object_mut()
                {
                    object.insert("cache_control".into(), anthropic_cache_control(ttl));
                }
                blocks.push(block);
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

fn anthropic_cache_control(ttl: crate::AnthropicCacheTtl) -> serde_json::Value {
    serde_json::json!({"type": "ephemeral", "ttl": ttl.wire()})
}

fn annotate_anthropic_message_boundary(
    wire_messages: &mut [serde_json::Value],
    source_messages: &[crate::Message],
    boundary: usize,
    ttl: crate::AnthropicCacheTtl,
) {
    if boundary == 0 || boundary > wire_messages.len() || boundary > source_messages.len() {
        return;
    }
    // Provider-opaque blocks may carry signatures over their exact object.
    // If one terminates the requested boundary, omit this breakpoint instead
    // of mutating or traversing that provider-owned value.
    if source_messages[boundary - 1]
        .blocks
        .last()
        .is_none_or(|block| matches!(block, Block::ProviderOpaque { .. }))
    {
        return;
    }
    let Some(content) = wire_messages[boundary - 1]
        .get_mut("content")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };
    if let Some(object) = content
        .last_mut()
        .and_then(serde_json::Value::as_object_mut)
    {
        object.insert("cache_control".into(), anthropic_cache_control(ttl));
    }
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

/// Builds one message's content array. Almost every block maps 1:1 through
/// [`content_block`]; the exception is a captured citation-signed TEXT block
/// (W-B): its opaque copy is the replay authority, so the normalized text
/// deltas that streamed the same characters are consumed instead of being
/// sent twice.
fn message_content(
    role: MessageRole,
    blocks: &[Block],
    attachments: &HashMap<&str, &str>,
) -> Result<Vec<serde_json::Value>, ProviderError> {
    let mut content = Vec::with_capacity(blocks.len());
    for block in blocks {
        if role == MessageRole::Assistant
            && let Block::ProviderOpaque { provider, data } = block
            && provider == crate::ANTHROPIC_PROVIDER_NAME
            && data.get("type").and_then(serde_json::Value::as_str) == Some("text")
        {
            consume_normalized_text_for_signed_block(&mut content, data)?;
            content.push(data.clone());
            continue;
        }
        content.push(content_block(role, block, attachments)?);
    }
    Ok(content)
}

/// A captured citation-signed text block arrives AFTER the normalized deltas
/// that streamed its characters (capture happens at the block's stop). Those
/// trailing plain-text entries are popped so the opaque block replays alone.
/// A rehydrated cross-turn message carries the opaque block with NO trailing
/// normalized text — zero pops is the tolerated shape; a PARTIAL match means
/// the capture disagrees with normalized history and is refused.
fn consume_normalized_text_for_signed_block(
    content: &mut Vec<serde_json::Value>,
    data: &serde_json::Value,
) -> Result<(), ProviderError> {
    let expected = data
        .get("text")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| invalid_request("Anthropic citation-signed text block has no text"))?;
    let is_plain_text = |value: &serde_json::Value| {
        value.get("type").and_then(serde_json::Value::as_str) == Some("text")
            && value.get("citations").is_none()
    };
    if !content.last().is_some_and(is_plain_text) {
        return Ok(());
    }
    let mut consumed = String::new();
    while consumed != expected {
        let piece = content
            .last()
            .filter(|value| is_plain_text(value))
            .and_then(|value| value.get("text").and_then(serde_json::Value::as_str))
            .ok_or_else(|| {
                invalid_request("Anthropic citation-signed text disagrees with normalized history")
            })?;
        let candidate = format!("{piece}{consumed}");
        if !expected.ends_with(candidate.as_str()) {
            return Err(invalid_request(
                "Anthropic citation-signed text disagrees with normalized history",
            ));
        }
        content.pop();
        consumed = candidate;
    }
    Ok(())
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
            call_id,
            preview,
            images,
            ..
        } if matches!(role, MessageRole::User | MessageRole::Tool) => {
            if images.is_empty() {
                return Ok(serde_json::json!({
                    "type": "tool_result",
                    "tool_use_id": call_id,
                    "content": preview,
                }));
            }
            let mut content = vec![serde_json::json!({"type": "text", "text": preview})];
            for image in images {
                if !crate::tool_image_media_type_supported(&image.media_type) {
                    return Err(invalid_request(format!(
                        "tool image {} has unsupported media type",
                        image.artifact
                    )));
                }
                let data = attachments.get(image.artifact.as_str()).ok_or_else(|| {
                    invalid_request(format!(
                        "tool image {} was not resolved before Anthropic shaping",
                        image.artifact
                    ))
                })?;
                content.push(serde_json::json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": image.media_type,
                        "data": data,
                    }
                }));
            }
            Ok(serde_json::json!({
                "type": "tool_result",
                "tool_use_id": call_id,
                "content": content,
            }))
        }
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
        Block::Attachment(AttachmentBlock::Pdf { artifact, .. }) if role == MessageRole::User => {
            let data = attachments.get(artifact.as_str()).ok_or_else(|| {
                invalid_request(format!(
                    "PDF attachment `{artifact}` has no resolved base64 data"
                ))
            })?;
            Ok(serde_json::json!({
                "type": "document",
                "source": {
                    "type": "base64",
                    "media_type": "application/pdf",
                    "data": data,
                }
            }))
        }
        Block::Attachment(AttachmentBlock::Pdf { .. }) => Err(invalid_request(
            "Anthropic document blocks are only valid in user messages",
        )),
        Block::Attachment(AttachmentBlock::PastedText { artifact, .. }) => Err(invalid_request(
            format!("pasted-text attachment `{artifact}` was not resolved by the prompt compiler"),
        )),
        Block::Attachment(AttachmentBlock::File { artifact, .. }) => Err(invalid_request(format!(
            "file attachment `{artifact}` was not resolved by the prompt compiler"
        ))),
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
                for item in self.accept_line(&line) {
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
            let items = self.accept_line(line.trim_end_matches('\r'));
            if !items.is_empty() {
                self.terminal = items
                    .iter()
                    .any(|item| matches!(item, Err(_) | Ok(StreamEvent::Finish { .. })));
                return items;
            }
        }
        if self.event_name.is_some() || !self.data_lines.is_empty() {
            let items = self.dispatch_event();
            if !items.is_empty() {
                self.terminal = items
                    .iter()
                    .any(|item| matches!(item, Err(_) | Ok(StreamEvent::Finish { .. })));
                return items;
            }
        }
        self.fail(stream_interrupted(
            "Anthropic SSE stream ended before a message_stop event",
        ))
    }

    pub(crate) const fn is_terminal(&self) -> bool {
        self.terminal
    }

    fn accept_line(&mut self, line: &str) -> Vec<ProviderStreamItem> {
        if line.is_empty() {
            return self.dispatch_event();
        }
        if line.starts_with(':') {
            return Vec::new();
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
        Vec::new()
    }

    fn dispatch_event(&mut self) -> Vec<ProviderStreamItem> {
        let event_name = self.event_name.take();
        if self.data_lines.is_empty() {
            return Vec::new();
        }
        let data = self.data_lines.join("\n");
        self.data_lines.clear();
        let Some(event_name) = event_name else {
            return vec![Err(malformed("Anthropic SSE data frame has no event name"))];
        };
        let value: serde_json::Value = match serde_json::from_str(&data) {
            Ok(value) => value,
            Err(error) => {
                return vec![Err(malformed(format!(
                    "Anthropic SSE `{event_name}` data is not valid JSON: {error}"
                )))];
            }
        };
        let data_type = value.get("type").and_then(serde_json::Value::as_str);
        if known_event_name(&event_name) && data_type != Some(event_name.as_str()) {
            return vec![Err(malformed(format!(
                "Anthropic SSE event `{event_name}` disagrees with data type `{}`",
                data_type.unwrap_or("<missing>")
            )))];
        }
        if !known_event_name(&event_name) {
            return Vec::new();
        }
        let event = match serde_json::from_value(value) {
            Ok(event) => event,
            Err(error) => {
                return vec![Err(malformed(format!(
                    "Anthropic SSE `{event_name}` frame has an invalid shape: {error}"
                )))];
            }
        };
        match self.state.apply(event) {
            Ok(events) => events.into_iter().map(Ok).collect(),
            Err(error) => vec![Err(error)],
        }
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

    fn apply(&mut self, event: WireEvent) -> Result<Vec<StreamEvent>, ProviderError> {
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
                Ok(Vec::new())
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
                    WireContentBlock::Text { text } => {
                        let item = (!text.is_empty())
                            .then(|| StreamEvent::TextDelta { text: text.clone() });
                        (
                            OpenBlock::Text {
                                text,
                                citations: Vec::new(),
                            },
                            item,
                        )
                    }
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
                    // W-B: a provider-executed tool call. It NEVER enters the
                    // local dispatch loop (no ToolCallStart); the block is
                    // accumulated for verbatim replay and surfaced as a
                    // display row at its stop.
                    WireContentBlock::ServerToolUse {
                        id,
                        name,
                        input,
                        extra,
                    } => (
                        OpenBlock::ServerTool {
                            id,
                            name,
                            input,
                            input_json: String::new(),
                            extra,
                        },
                        None,
                    ),
                    WireContentBlock::WebSearchToolResult {
                        tool_use_id,
                        content,
                        extra,
                    } => (
                        OpenBlock::ServerResult {
                            kind: "web_search_tool_result",
                            tool_use_id,
                            content,
                            extra,
                        },
                        None,
                    ),
                    WireContentBlock::WebFetchToolResult {
                        tool_use_id,
                        content,
                        extra,
                    } => (
                        OpenBlock::ServerResult {
                            kind: "web_fetch_tool_result",
                            tool_use_id,
                            content,
                            extra,
                        },
                        None,
                    ),
                    // G3 (LT1): thinking blocks accumulate their text AND
                    // signature for verbatim tool-loop replay. Display law is
                    // unchanged — only DELTAS are emitted as ReasoningDelta;
                    // the start content merely seeds the replay accumulator.
                    WireContentBlock::Thinking { thinking } => (
                        OpenBlock::Thinking {
                            thinking,
                            signature: String::new(),
                        },
                        None,
                    ),
                    WireContentBlock::RedactedThinking { data } => {
                        (OpenBlock::Redacted { data }, None)
                    }
                    WireContentBlock::Fallback | WireContentBlock::Unknown => {
                        (OpenBlock::Opaque, None)
                    }
                };
                self.open_blocks.insert(index, block);
                Ok(item.into_iter().collect())
            }
            WireEvent::ContentBlockDelta { index, delta } => {
                self.require_started("content_block_delta")?;
                self.require_before_message_delta("content_block_delta")?;
                let block = self.open_blocks.get_mut(&index).ok_or_else(|| {
                    malformed(format!(
                        "Anthropic delta references unopened content block index {index}"
                    ))
                })?;
                match (block, delta) {
                    (OpenBlock::Text { text: accum, .. }, WireDelta::Text { text }) => {
                        accum.push_str(&text);
                        Ok(vec![StreamEvent::TextDelta { text }])
                    }
                    // W-B: citation fragments accumulate on their text block
                    // for verbatim replay; they are not display deltas.
                    (OpenBlock::Text { citations, .. }, WireDelta::Citations { citation }) => {
                        citations.push(citation);
                        Ok(Vec::new())
                    }
                    (OpenBlock::Tool { call_id }, WireDelta::InputJson { partial_json }) => {
                        Ok(vec![StreamEvent::ToolCallArgsDelta {
                            call_id: call_id.clone(),
                            args_fragment: partial_json,
                        }])
                    }
                    // W-B: a server tool call streams its input exactly like
                    // a client tool call, but into the replay accumulator.
                    (
                        OpenBlock::ServerTool { input_json, .. },
                        WireDelta::InputJson { partial_json },
                    ) => {
                        input_json.push_str(&partial_json);
                        Ok(Vec::new())
                    }
                    // Display AND capture: the delta streams to the UI as
                    // reasoning and joins the verbatim replay accumulator.
                    (
                        OpenBlock::Thinking { thinking, .. },
                        WireDelta::Thinking { thinking: delta },
                    ) => {
                        thinking.push_str(&delta);
                        Ok(vec![StreamEvent::ReasoningDelta { text: delta }])
                    }
                    // signature_delta fragments ACCUMULATE (LT1): the block's
                    // replayable signature is their concatenation.
                    (
                        OpenBlock::Thinking { signature, .. },
                        WireDelta::Signature { signature: delta },
                    ) => {
                        signature.push_str(&delta);
                        Ok(Vec::new())
                    }
                    (_, WireDelta::Unknown) => Ok(Vec::new()),
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
                        Ok(vec![StreamEvent::ToolCallEnd { call_id }])
                    }
                    // W-B (LW2): the finished server tool call is captured
                    // VERBATIM — id, name, streamed input, and every unknown
                    // sibling field — for the API's mandatory echo, then
                    // surfaced as a display row.
                    Some(OpenBlock::ServerTool {
                        id,
                        name,
                        input,
                        input_json,
                        extra,
                    }) => {
                        let input = if input_json.is_empty() {
                            input
                        } else {
                            serde_json::from_str(&input_json).map_err(|error| {
                                malformed(format!(
                                    "Anthropic server tool input is not valid JSON: {error}"
                                ))
                            })?
                        };
                        let mut raw = serde_json::Map::new();
                        raw.insert("type".into(), serde_json::json!("server_tool_use"));
                        raw.insert("id".into(), serde_json::Value::String(id.clone()));
                        raw.insert("name".into(), serde_json::Value::String(name.clone()));
                        raw.insert("input".into(), input.clone());
                        raw.extend(extra);
                        Ok(vec![
                            StreamEvent::ProviderOpaque {
                                provider: crate::ANTHROPIC_PROVIDER_NAME.into(),
                                data: serde_json::Value::Object(raw),
                            },
                            StreamEvent::ServerToolUse {
                                call_id: id,
                                name,
                                args: input,
                            },
                        ])
                    }
                    // W-B (LW2/LW3): the result block — encrypted_content and
                    // all — replays verbatim; the display row decodes the
                    // content TOLERANTLY (error content is an OBJECT with an
                    // error_code, search success is a LIST, fetch success is
                    // a web_fetch_result object; an empty list is zero
                    // results, not an error).
                    Some(OpenBlock::ServerResult {
                        kind,
                        tool_use_id,
                        content,
                        extra,
                    }) => {
                        let (preview, is_error) = server_result_preview(&content);
                        let mut raw = serde_json::Map::new();
                        raw.insert("type".into(), serde_json::json!(kind));
                        raw.insert(
                            "tool_use_id".into(),
                            serde_json::Value::String(tool_use_id.clone()),
                        );
                        raw.insert("content".into(), content);
                        raw.extend(extra);
                        Ok(vec![
                            StreamEvent::ProviderOpaque {
                                provider: crate::ANTHROPIC_PROVIDER_NAME.into(),
                                data: serde_json::Value::Object(raw),
                            },
                            StreamEvent::ServerToolResult {
                                call_id: tool_use_id,
                                preview,
                                is_error,
                            },
                        ])
                    }
                    // G3 (LT1): a SIGNED thinking block becomes a
                    // provider-opaque fact carrying the EXACT wire shape the
                    // Messages API requires replayed verbatim within a
                    // tool-use turn. An unsigned block (display-only
                    // thinking) is not replayable and captures nothing —
                    // today's behavior.
                    Some(OpenBlock::Thinking {
                        thinking,
                        signature,
                    }) => Ok((!signature.is_empty())
                        .then(|| StreamEvent::ProviderOpaque {
                            provider: crate::ANTHROPIC_PROVIDER_NAME.into(),
                            data: serde_json::json!({
                                "type": "thinking",
                                "thinking": thinking,
                                "signature": signature,
                            }),
                        })
                        .into_iter()
                        .collect()),
                    // redacted_thinking replays as-is; the classic bug this
                    // pins against is filtering `type == "thinking"` and
                    // dropping these (a live 400).
                    Some(OpenBlock::Redacted { data }) => Ok((!data.is_empty())
                        .then(|| StreamEvent::ProviderOpaque {
                            provider: crate::ANTHROPIC_PROVIDER_NAME.into(),
                            data: serde_json::json!({
                                "type": "redacted_thinking",
                                "data": data,
                            }),
                        })
                        .into_iter()
                        .collect()),
                    // W-B: a citation-carrying text block is captured with
                    // its citations (every encrypted_index included) so the
                    // follow-up request can echo it verbatim; the sources it
                    // cites surface for display. A plain text block captures
                    // nothing — today's behavior.
                    Some(OpenBlock::Text { text, citations }) => {
                        if citations.is_empty() {
                            return Ok(Vec::new());
                        }
                        let sources = citation_sources(&citations);
                        let mut events = vec![StreamEvent::ProviderOpaque {
                            provider: crate::ANTHROPIC_PROVIDER_NAME.into(),
                            data: serde_json::json!({
                                "type": "text",
                                "text": text,
                                "citations": citations,
                            }),
                        }];
                        if !sources.is_empty() {
                            events.push(StreamEvent::WebSources { sources });
                        }
                        Ok(events)
                    }
                    Some(OpenBlock::Opaque) => Ok(Vec::new()),
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
                    return Ok(Vec::new());
                };
                self.input.update(&usage)?;
                let input = self
                    .input
                    .input_tokens
                    .unwrap_or(0)
                    .checked_add(self.input.cache_creation_input_tokens.unwrap_or(0))
                    .ok_or_else(|| malformed("Anthropic input usage counter overflowed u64"))?;
                let normalized = self.input.normalized(usage.output_tokens.unwrap_or(0))?;
                Ok(vec![StreamEvent::UsageUpdate(Usage {
                    input,
                    output: usage.output_tokens.unwrap_or(0),
                    reasoning: 0,
                    cached: normalized.cache_read_input,
                    source: UsageSource::ProviderReported,
                    account: self.account.clone(),
                    accounts: Vec::new(),
                    normalized: Some(normalized),
                    scope: None,
                    cache_cost: None,
                })])
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
                Ok(vec![StreamEvent::Finish { reason }])
            }
            WireEvent::Ping | WireEvent::Unknown => Ok(Vec::new()),
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
    input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_write_5m_input_tokens: Option<u64>,
    cache_write_1h_input_tokens: Option<u64>,
}

impl InputUsage {
    fn update(&mut self, usage: &WireUsage) -> Result<(), ProviderError> {
        if let Some(value) = usage.input_tokens {
            self.input_tokens = Some(value);
        }
        if let Some(value) = usage.cache_creation_input_tokens {
            self.cache_creation_input_tokens = Some(value);
        }
        if let Some(value) = usage.cache_read_input_tokens {
            self.cache_read_input_tokens = Some(value);
        }
        if let Some(cache_creation) = &usage.cache_creation {
            if let Some(value) = cache_creation.ephemeral_5m_input_tokens {
                self.cache_write_5m_input_tokens = Some(value);
            }
            if let Some(value) = cache_creation.ephemeral_1h_input_tokens {
                self.cache_write_1h_input_tokens = Some(value);
            }
        }
        self.input_tokens
            .unwrap_or(0)
            .checked_add(self.cache_creation_input_tokens.unwrap_or(0))
            .ok_or_else(|| malformed("Anthropic input usage counter overflowed u64"))?;
        Ok(())
    }

    fn normalized(&self, billed_output: u64) -> Result<NormalizedUsage, ProviderError> {
        let input = self.input_tokens.unwrap_or(0);
        let write = self.cache_creation_input_tokens.unwrap_or(0);
        let read = self.cache_read_input_tokens.unwrap_or(0);
        let uncached = input
            .checked_add(write)
            .ok_or_else(|| malformed("Anthropic uncached usage counter overflowed u64"))?;
        let logical = uncached
            .checked_add(read)
            .ok_or_else(|| malformed("Anthropic logical usage counter overflowed u64"))?;
        let cache_status = if self.input_tokens.is_some()
            && self.cache_creation_input_tokens.is_some()
            && self.cache_read_input_tokens.is_some()
        {
            CacheStatAvailability::Present
        } else {
            CacheStatAvailability::Unavailable
        };
        let ttl_sum = self
            .cache_write_5m_input_tokens
            .zip(self.cache_write_1h_input_tokens)
            .and_then(|(five, one)| five.checked_add(one));
        let ttl_present = ttl_sum == self.cache_creation_input_tokens;
        Ok(if cache_status == CacheStatAvailability::Present {
            NormalizedUsage {
                logical_input: logical,
                uncached_input: uncached,
                cache_read_input: read,
                cache_write_input: write,
                cache_write_5m_input: self.cache_write_5m_input_tokens.unwrap_or(0),
                cache_write_1h_input: self.cache_write_1h_input_tokens.unwrap_or(0),
                billed_output,
                reasoning_accounting: ReasoningAccounting::Unavailable,
                cache_status,
                cache_write_status: CacheStatAvailability::Present,
                cache_write_ttl_status: if ttl_present {
                    CacheStatAvailability::Present
                } else {
                    CacheStatAvailability::Unavailable
                },
                cache_telemetry_input: logical,
                ..NormalizedUsage::default()
            }
        } else {
            NormalizedUsage {
                logical_input: logical,
                uncached_input: logical,
                billed_output,
                cache_write_input: write,
                cache_write_status: if self.cache_creation_input_tokens.is_some() {
                    CacheStatAvailability::Present
                } else {
                    CacheStatAvailability::Unavailable
                },
                ..NormalizedUsage::default()
            }
        })
    }
}

#[derive(Debug)]
enum OpenBlock {
    /// Text accumulates its characters and any citation fragments (W-B): a
    /// citation-carrying block must replay verbatim, encrypted_index and all.
    Text {
        text: String,
        citations: Vec<serde_json::Value>,
    },
    Tool {
        call_id: String,
    },
    /// A provider-executed tool call accumulating its streamed input plus
    /// every unknown sibling field for verbatim replay (W-B).
    ServerTool {
        id: String,
        name: String,
        input: serde_json::Value,
        input_json: String,
        extra: serde_json::Map<String, serde_json::Value>,
    },
    /// A server tool result block held whole for verbatim replay (W-B).
    ServerResult {
        kind: &'static str,
        tool_use_id: String,
        content: serde_json::Value,
        extra: serde_json::Map<String, serde_json::Value>,
    },
    /// Thinking accumulates text + signature for verbatim replay (G3/LT1).
    Thinking {
        thinking: String,
        signature: String,
    },
    /// redacted_thinking carries its encrypted payload from the start event.
    Redacted {
        data: String,
    },
    Opaque,
}

/// Bounded display decode for one server tool result's `content` (LW3):
/// error content is an OBJECT carrying `error_code`; web_search success is a
/// LIST (empty = zero results, not an error); web_fetch success is a
/// `web_fetch_result` object. Anything else stays a tolerant "result".
fn server_result_preview(content: &serde_json::Value) -> (String, bool) {
    if let Some(items) = content.as_array() {
        let count = items.len();
        let preview = if count == 1 {
            "1 result".to_owned()
        } else {
            format!("{count} results")
        };
        return (preview, false);
    }
    if let Some(object) = content.as_object() {
        let kind = object.get("type").and_then(serde_json::Value::as_str);
        if kind.is_some_and(|kind| kind.ends_with("_error")) {
            let code = object
                .get("error_code")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("error");
            return (code.to_owned(), true);
        }
        if let Some(url) = object.get("url").and_then(serde_json::Value::as_str) {
            return (format!("fetched {url}"), false);
        }
    }
    ("result".to_owned(), false)
}

/// Extracts the display sources cited by one text block's citations,
/// deduplicated by URL; tolerant to citation shapes without url/title.
fn citation_sources(citations: &[serde_json::Value]) -> Vec<WebSource> {
    let mut sources: Vec<WebSource> = Vec::new();
    for citation in citations {
        let Some(url) = citation.get("url").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if sources.iter().any(|source| source.url == url) {
            continue;
        }
        sources.push(WebSource {
            url: url.to_owned(),
            title: citation
                .get("title")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
        });
    }
    sources
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
    #[serde(default)]
    cache_creation: Option<WireCacheCreation>,
}

#[derive(Debug, Deserialize)]
struct WireCacheCreation {
    #[serde(default)]
    ephemeral_5m_input_tokens: Option<u64>,
    #[serde(default)]
    ephemeral_1h_input_tokens: Option<u64>,
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
    /// W-B: a provider-executed tool call. `extra` flatten-captures every
    /// unknown sibling field so the block can be reconstructed verbatim.
    ServerToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: serde_json::Value,
        #[serde(flatten)]
        extra: serde_json::Map<String, serde_json::Value>,
    },
    WebSearchToolResult {
        tool_use_id: String,
        #[serde(default)]
        content: serde_json::Value,
        #[serde(flatten)]
        extra: serde_json::Map<String, serde_json::Value>,
    },
    WebFetchToolResult {
        tool_use_id: String,
        #[serde(default)]
        content: serde_json::Value,
        #[serde(flatten)]
        extra: serde_json::Map<String, serde_json::Value>,
    },
    Thinking {
        #[serde(default)]
        thinking: String,
    },
    RedactedThinking {
        #[serde(default)]
        data: String,
    },
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
    Signature {
        #[serde(default)]
        signature: String,
    },
    /// W-B: one citation landing on an open text block, kept raw so its
    /// `encrypted_index` replays byte-exactly.
    #[serde(rename = "citations_delta")]
    Citations { citation: serde_json::Value },
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
    let kind = if crate::anthropic::anthropic_billing_exhausted(&error.kind, &error.message) {
        ProviderErrorKind::QuotaExhausted
    } else if is_anthropic_context_error(&error.kind, &error.message) {
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
        ProviderErrorKind::QuotaExhausted => "a quota/credit-exhaustion error",
        ProviderErrorKind::StreamInterrupted => "a stream-interruption error",
        ProviderErrorKind::ConnectionConfiguration => "a connection-configuration error",
    }
}

fn normalize_stop_reason(reason: &str) -> Result<FinishReason, ProviderError> {
    match reason {
        "end_turn" | "stop_sequence" => Ok(FinishReason::EndTurn),
        // W-B (LW2): `pause_turn` is a CONTINUATION signal, never a terminal
        // end — the turn engine resends the paused assistant message
        // unchanged.
        "pause_turn" => Ok(FinishReason::PauseTurn),
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

fn stream_interrupted(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ProviderErrorKind::StreamInterrupted, message)
}
