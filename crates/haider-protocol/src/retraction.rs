//! Additive append-only prompt retraction and first-response arbitration.

use crate::envelope::RawEnvelope;
use crate::ids::{NodeId, RunId, SessionId};
use crate::tool::AttachmentBlock;
use serde::{Deserialize, Serialize};

pub const TURN_RETRACT_FEATURE: &str = "turn_retract_v1";

/// Content, rather than transport/usage metadata, closes the editable window.
#[must_use]
pub fn is_response_delta(event: &crate::provider::StreamEvent) -> bool {
    use crate::provider::StreamEvent;
    match event {
        StreamEvent::TextDelta { text } | StreamEvent::ReasoningDelta { text } => !text.is_empty(),
        StreamEvent::RefusalDelta { text } => !text.is_empty(),
        StreamEvent::ToolCallArgsDelta { args_fragment, .. } => !args_fragment.is_empty(),
        StreamEvent::WebSources { sources } => !sources.is_empty(),
        StreamEvent::NetworkUnavailable
        | StreamEvent::NetworkRestored
        | StreamEvent::UsageUpdate(_)
        | StreamEvent::Finish { .. } => false,
        StreamEvent::ProviderOpaque { .. }
        | StreamEvent::ToolCallStart { .. }
        | StreamEvent::ToolCallEnd { .. }
        | StreamEvent::ServerToolUse { .. }
        | StreamEvent::ServerToolResult { .. } => true,
    }
}

/// Retains the exact accepted draft, including immutable attachment references.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptRetractedV1 {
    pub prompt_seq: u64,
    pub prompt_node_id: NodeId,
    pub text: String,
    pub attachments: Vec<AttachmentBlock>,
}

impl PromptRetractedV1 {
    pub fn to_payload_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        let mut value = serde_json::to_value(self)?;
        value["type"] = "prompt_retracted".into();
        Ok(value)
    }

    #[must_use]
    pub fn from_payload_value(value: &serde_json::Value) -> Option<Self> {
        (value.get("type")?.as_str()? == "prompt_retracted")
            .then(|| serde_json::from_value(value.clone()).ok())?
    }
}

/// Durable receipt response; a retry returns the original restoration draft.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetractedTurn {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub prompt_seq: u64,
    pub retracted_seq: u64,
    pub text: String,
    pub attachments: Vec<AttachmentBlock>,
}

/// The store serializes this fence against the retraction transaction before
/// the actor makes any semantic provider response visible.
#[must_use]
pub fn response_started_payload(delta: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"type": "response_started", "delta": delta})
}

#[must_use]
pub fn response_was_discarded(envelope: &RawEnvelope) -> bool {
    envelope
        .payload
        .get("type")
        .and_then(serde_json::Value::as_str)
        == Some("response_delta_discarded")
}
