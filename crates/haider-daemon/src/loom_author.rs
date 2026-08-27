//! Loom authoring helpers: prose seed -> editable typed text -> validation.
//!
//! Durable mutation remains in the existing Loom registry. These helpers are
//! pure over one registry snapshot so `loom.author.revise` and
//! `loom.author.confirm` can run the same validation without creating a
//! second compiler or mutable shadow registry.

use crate::worker::ProviderFactory;
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::loom::{
    LoomAgentType, LoomAuthorDraft, LoomAuthorKind, ValidatedLoomAuthorSpec,
    validate_loom_author_text,
};
use haider_protocol::provider::StreamEvent;
use haider_protocol::session::SessionMetadataV1;
use haider_provider::{Message, TurnRequest};
use std::collections::HashMap;

const LOOM_AUTHOR_PROSE_MAX_BYTES: usize = 8 * 1024;

const LOOM_AUTHOR_DRAFT_MAX_BYTES: usize = haider_protocol::loom::LOOM_AUTHOR_TEXT_MAX_BYTES;
pub(crate) const LOOM_AUTHOR_SESSION_MAX: usize = 64;

#[derive(Clone)]
pub(crate) struct LoomAuthorSession {
    pub(crate) kind: LoomAuthorKind,
    pub(crate) revision: u64,
    pub(crate) text: String,
    pub(crate) valid: bool,
    pub(crate) confirming: bool,
    pub(crate) confirmed: Option<haider_protocol::loom::LoomAuthorConfirmed>,
    pub(crate) confirmed_input_text: Option<String>,
    pub(crate) updated_at: std::time::Instant,
}

impl LoomAuthorSession {
    pub(crate) fn pending(kind: LoomAuthorKind) -> Self {
        Self {
            kind,
            revision: 0,
            text: String::new(),
            valid: false,
            confirming: true,
            confirmed: None,
            confirmed_input_text: None,
            updated_at: std::time::Instant::now(),
        }
    }

    pub(crate) fn from_draft(draft: &LoomAuthorDraft) -> Self {
        Self {
            kind: draft.kind,
            revision: draft.revision,
            text: draft.text.clone(),
            valid: draft.errors.is_empty(),
            confirming: false,
            confirmed: None,
            confirmed_input_text: None,
            updated_at: std::time::Instant::now(),
        }
    }
}

pub(crate) fn validate_prose(prose: &str) -> Result<(), HaiderError> {
    if prose.trim().is_empty() || prose.len() > LOOM_AUTHOR_PROSE_MAX_BYTES {
        return Err(HaiderError::new(
            ErrorCode::InvalidArgument,
            format!("Loom authoring prose must be 1..={LOOM_AUTHOR_PROSE_MAX_BYTES} bytes"),
            false,
        ));
    }
    Ok(())
}

pub(crate) async fn draft_from_prose(
    authoring_id: String,
    kind: LoomAuthorKind,
    prose: &str,
    agent_types: &[LoomAgentType],
    metadata: &SessionMetadataV1,
    provider_factory: &dyn ProviderFactory,
) -> Result<LoomAuthorDraft, HaiderError> {
    validate_prose(prose)?;
    let prose = prose.trim();
    let resolved = provider_factory.resolve_for_turn(metadata).await?;
    if resolved.provider_name != metadata.provider {
        return Err(HaiderError::new(
            ErrorCode::ProviderError,
            "provider factory returned a different provider for Loom authoring",
            false,
        ));
    }
    let registry = agent_types
        .iter()
        .map(|record| {
            serde_json::json!({
                "id": record.id,
                "in_type": record.in_type,
                "out_type": record.out_type,
            })
        })
        .collect::<Vec<_>>();
    let kind_name = match kind {
        LoomAuthorKind::AgentType => "agent_type",
        LoomAuthorKind::Workflow => "workflow",
    };
    let prompt = format!(
        "Draft a typed Loom {kind_name} from the user's prose. Return ONLY one JSON object, with no markdown fence or explanation.\n\
         Agent-type schema: {{\"kind\":\"agent_type\",\"id\":string,\"name\":string,\"job\":string,\"in_type\":string,\"out_type\":string,\"capability_keys\":[\"cli:name\"|\"api:host\"],\"grants\":[same],\"denials\":[same],\"skills\":[string],\"scripts\":[string],\"color\":\"\",\"glyph\":\"\"}}. Every capability key must occur exactly once in grants or denials.\n\
         Workflow schema: {{\"kind\":\"workflow\",\"id\":string,\"in_type\":string,\"out_type\":string,\"nodes\":[{{\"id\":string,\"agent_type\":string|null,\"task\":quote-free single-line string <=200 bytes,\"in_type\":string,\"out_type\":string,\"gate\":\"command\"|\"review\"|\"human\"|\"all_of\",\"depends_on\":[earlier node ids],\"back_edge\":self or earlier node id|null,\"evidence\":{{\"protocol\":\"instruct_pipe_v1\",\"tool\":\"graph_evidence\",\"required_green\":positive integer}}}}]}}. Use explicit depends_on for every non-root node; forks share a dependency, joins name all branch dependencies, and back edges only target the current node or an earlier ancestor. Control nodes have null agent_type and preserve their derived input type. Registered agent types and exact signatures: {}.\n\
         Requested prose as a JSON string: {}",
        serde_json::Value::Array(registry),
        serde_json::Value::String(prose.to_owned())
    );
    let request = TurnRequest {
        messages: vec![Message::user_text(prompt)],
        model: resolved.model,
        max_tokens: metadata.max_tokens.clamp(512, 8_192),
        system_prompt: Some(
            "You are the Loom typed-spec drafting engine. Preserve least privilege, explicit evidence contracts, and typed graph flow. Emit JSON only.".to_owned(),
        ),
        tools: Vec::new(),
        attachments: Vec::new(),
        cache_metadata: None,
    };
    let mut stream = resolved
        .provider
        .stream_turn(request)
        .await
        .map_err(provider_draft_error)?;
    let mut output = String::new();
    let mut finished = false;
    while let Some(item) = stream.recv().await {
        match item.map_err(provider_draft_error)? {
            StreamEvent::TextDelta { text } => {
                if finished {
                    return Err(HaiderError::new(
                        ErrorCode::ProviderError,
                        "AI Loom draft emitted text after finish",
                        false,
                    ));
                }
                if output.len().saturating_add(text.len()) > LOOM_AUTHOR_DRAFT_MAX_BYTES {
                    return Err(HaiderError::new(
                        ErrorCode::InvalidArgument,
                        "AI Loom draft exceeded the 64 KiB authoring limit",
                        false,
                    ));
                }
                output.push_str(&text);
            }
            StreamEvent::RefusalDelta { .. } => {
                return Err(HaiderError::new(
                    ErrorCode::PermissionDenied,
                    "AI declined the Loom draft",
                    false,
                ));
            }
            StreamEvent::Finish { reason } => {
                if finished {
                    return Err(HaiderError::new(
                        ErrorCode::ProviderError,
                        "AI Loom draft emitted more than one finish event",
                        false,
                    ));
                }
                if reason != haider_protocol::provider::FinishReason::EndTurn {
                    return Err(HaiderError::new(
                        ErrorCode::ProviderError,
                        format!("AI Loom draft ended without a complete document: {reason:?}"),
                        reason == haider_protocol::provider::FinishReason::MaxTokens,
                    ));
                }
                finished = true;
            }
            StreamEvent::ReasoningDelta { .. }
            | StreamEvent::UsageUpdate(_)
            | StreamEvent::ProviderOpaque { .. } => {}
            StreamEvent::ToolCallStart { .. }
            | StreamEvent::ToolCallArgsDelta { .. }
            | StreamEvent::ToolCallEnd { .. }
            | StreamEvent::ServerToolUse { .. }
            | StreamEvent::ServerToolResult { .. }
            | StreamEvent::WebSources { .. } => {
                return Err(HaiderError::new(
                    ErrorCode::InvalidArgument,
                    "AI Loom draft attempted an unsupported tool call",
                    false,
                ));
            }
        }
    }
    if !finished {
        return Err(HaiderError::new(
            ErrorCode::ProviderError,
            "AI Loom draft stream ended before finish",
            true,
        ));
    }
    let text = strip_json_fence(output.trim()).to_owned();
    if text.is_empty() {
        return Err(HaiderError::new(
            ErrorCode::InvalidArgument,
            "AI returned an empty Loom draft",
            false,
        ));
    }
    Ok(revise(authoring_id, 1, kind, text, agent_types))
}

pub(crate) fn revise(
    authoring_id: String,
    revision: u64,
    kind: LoomAuthorKind,
    text: String,
    agent_types: &[LoomAgentType],
) -> LoomAuthorDraft {
    let registry = agent_types
        .iter()
        .map(|record| (record.id.as_str(), record.signature()))
        .collect::<HashMap<_, _>>();
    let errors = validate_loom_author_text(&text, kind, |id| registry.get(id).cloned())
        .err()
        .unwrap_or_default();
    LoomAuthorDraft {
        authoring_id,
        revision,
        kind,
        text,
        errors,
    }
}

pub(crate) fn validate(
    text: &str,
    kind: LoomAuthorKind,
    agent_types: &[LoomAgentType],
) -> Result<ValidatedLoomAuthorSpec, Vec<haider_protocol::loom::LoomAuthorValidationError>> {
    let registry = agent_types
        .iter()
        .map(|record| (record.id.as_str(), record.signature()))
        .collect::<HashMap<_, _>>();
    validate_loom_author_text(text, kind, |id| registry.get(id).cloned())
}

fn provider_draft_error(error: haider_provider::ProviderError) -> HaiderError {
    HaiderError::new(
        ErrorCode::Internal,
        format!("AI Loom drafting failed: {}", error.message),
        error.retryable,
    )
}

fn strip_json_fence(text: &str) -> &str {
    let Some(body) = text
        .strip_prefix("```json")
        .or_else(|| text.strip_prefix("```"))
    else {
        return text;
    };
    body.trim()
        .strip_suffix("```")
        .map_or(body.trim(), str::trim)
}
