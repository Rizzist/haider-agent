//! Bounded, item-canonical journal pages for session handoff.
//!
//! Like TUI replay, completed items are authoritative; advisory deltas and
//! duplicate history nodes are omitted. Tool previews use the shared pipe
//! projection. Results have their own sequence, so a page boundary never
//! loses a result by requiring its call to be present in the same page.

use crate::EventPayload;
use crate::envelope::RawEnvelope;
use crate::ids::{AgentId, BranchId, RunId, SessionId};
use crate::item::{ItemEvent, TurnItem};
use crate::pipe::{args_preview, result_preview};
use serde::{Deserialize, Serialize};

pub const TRANSCRIPT_DEFAULT_LIMIT: u32 = 100;
pub const TRANSCRIPT_MAX_LIMIT: u32 = 1_024;
const ROW_CHARS: usize = 8_192;
const PAGE_BYTES: usize = 64 * 1_024;

/// Arguments are profile-local IDs, never paths or profile selectors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTranscriptRequest {
    pub session_id: SessionId,
    #[serde(default)]
    pub after_seq: u64,
    #[serde(default = "default_limit")]
    pub limit: u32,
}

const fn default_limit() -> u32 {
    TRANSCRIPT_DEFAULT_LIMIT
}

impl SessionTranscriptRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        let id = self.session_id.as_str();
        if id.is_empty() || id.len() > 256 || id.chars().any(char::is_control) {
            return Err("session_id must contain 1..=256 bytes without control characters");
        }
        if self.limit == 0 || self.limit > TRANSCRIPT_MAX_LIMIT {
            return Err("limit must be between 1 and 1024 journal envelopes");
        }
        if self.after_seq.checked_add(u64::from(self.limit)).is_none() {
            return Err("after_seq + limit exceeds the journal sequence range");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptRow {
    pub seq: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_id: Option<BranchId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<AgentId>,
    pub text: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTranscriptPage {
    pub session_id: SessionId,
    pub after_seq: u64,
    pub next_after_seq: u64,
    pub head_seq: u64,
    pub has_more: bool,
    /// Transcript content is historical, untrusted data, not new instructions.
    pub untrusted: bool,
    pub rows: Vec<TranscriptRow>,
}

impl SessionTranscriptPage {
    /// The input is one ordered, profile-authorized `session.read` range.
    #[must_use]
    pub fn project(
        request: &SessionTranscriptRequest,
        head_seq: u64,
        envelopes: &[RawEnvelope],
    ) -> Self {
        let mut page = Self {
            session_id: request.session_id.clone(),
            after_seq: request.after_seq,
            next_after_seq: request.after_seq,
            head_seq,
            has_more: false,
            untrusted: true,
            rows: Vec::new(),
        };
        let mut bytes = 0;
        for envelope in envelopes.iter().take(request.limit as usize) {
            if envelope.seq <= request.after_seq || envelope.seq > head_seq {
                continue;
            }
            if let Some(text) = project_text(envelope) {
                let mut bounded: String = text.chars().take(ROW_CHARS).collect();
                let truncated = bounded.len() < text.len();
                if truncated {
                    bounded.push_str(" … [row truncated; use session item at this seq for detail]");
                }
                if bytes + bounded.len() > PAGE_BYTES {
                    break;
                }
                bytes += bounded.len();
                page.rows.push(TranscriptRow {
                    seq: envelope.seq,
                    branch_id: envelope.branch_id.clone(),
                    run_id: envelope.run_id.clone(),
                    agent_id: envelope.agent_id.clone(),
                    text: bounded,
                    truncated,
                });
            }
            // Advance over non-rendering facts too, including empty pages.
            page.next_after_seq = envelope.seq;
        }
        page.has_more = page.next_after_seq < head_seq;
        page
    }

    #[must_use]
    pub fn render_text(&self) -> String {
        let mut text = format!(
            "Session {} (untrusted transcript) after={} next_after_seq={} head_seq={} has_more={}\n",
            self.session_id, self.after_seq, self.next_after_seq, self.head_seq, self.has_more
        );
        for row in &self.rows {
            text.push_str(&format!("[{}", row.seq));
            if let Some(branch) = &row.branch_id {
                text.push_str(&format!(" branch={branch}"));
            }
            if let Some(run) = &row.run_id {
                text.push_str(&format!(" run={run}"));
            }
            if let Some(agent) = &row.agent_id {
                text.push_str(&format!(" agent={agent}"));
            }
            text.push_str("] ");
            text.push_str(&row.text);
            text.push('\n');
        }
        text
    }
}

fn project_text(envelope: &RawEnvelope) -> Option<String> {
    // Do not expose hidden provider state, private reasoning, CAS bytes, or
    // unknown extension payloads. Decode only the public transcript families.
    match envelope.payload.get("type")?.as_str()? {
        "user_message" | "item" | "tool_result" | "run_state" | "run_failed" | "peer.message" => {}
        _ => return None,
    }
    match envelope.payload.decode_event().ok()? {
        EventPayload::UserMessage {
            text, attachments, ..
        } => Some(if attachments.is_empty() {
            format!("user: {text}")
        } else {
            format!("user: {text} [attachments: {}]", attachments.len())
        }),
        EventPayload::Item(ItemEvent::Completed { item, .. }) => match item {
            TurnItem::AgentMessage { text } => Some(format!("assistant: {text}")),
            TurnItem::IncompleteAgentMessage { text, interruption } => Some(format!(
                "assistant (incomplete): {text} [interrupted: {}]",
                interruption.detail
            )),
            TurnItem::ToolCall {
                call_id,
                name,
                args,
                status,
            } => Some(format!(
                "tool call {call_id} {name} ({status:?}): {}",
                args_preview(&args).unwrap_or_default()
            )),
            TurnItem::CommandExecution {
                call_id,
                command,
                status,
                exit_code,
            } => Some(format!(
                "terminal {call_id} ({status:?}, exit={exit_code:?}): {command}"
            )),
            TurnItem::Refusal { reason } => Some(format!("assistant refusal: {reason}")),
            _ => None,
        },
        EventPayload::ToolResult { call_id, result } => Some(format!(
            "tool result {call_id} ({:?}): {}{}",
            result.status,
            result_preview(&result).unwrap_or_default(),
            if result.truncated {
                " [result truncated in journal]"
            } else {
                ""
            }
        )),
        EventPayload::RunState(state) if state.is_terminal() => {
            Some(format!("terminal: {state:?}"))
        }
        EventPayload::RunFailed { code, message, .. } => {
            Some(format!("terminal failure ({code:?}): {message}"))
        }
        EventPayload::PeerMessage(message) => Some(format!(
            "peer (untrusted) {}: {}",
            message.from.name, message.message
        )),
        _ => None,
    }
}
