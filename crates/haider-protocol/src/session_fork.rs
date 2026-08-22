//! Auditable session-level fork provenance and metafork prompt omissions.
//!
//! A metafork never deletes copied journal rows. The selected child copies
//! remain durable and are marked `render.prompt = omit`; this additive fact
//! records why, what, and where they were omitted.

use crate::ids::{BranchId, EventId, NodeId, SessionId};
use serde::{Deserialize, Serialize};

fn is_false(value: &bool) -> bool {
    !*value
}

/// One exact prompt-visible source event shown in a removal review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMetaforkReviewEvent {
    pub source_seq: u64,
    pub source_event_id: EventId,
    pub payload_kind: String,
    pub excerpt: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub excerpt_truncated: bool,
}

/// One model-proposed inclusive source-journal range to omit from the child
/// prompt. Ranges are reviewed before commit and must not overlap; the
/// durable removal record is normalized to one entry per copied envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMetaforkRemoval {
    pub from_seq: u64,
    pub through_seq: u64,
    pub reason: String,
    /// Bounded, daemon-rendered source excerpt shown on the review surface.
    /// Exact source/child event coordinates remain the durable authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    /// Daemon-derived, exhaustive roster of prompt-visible events selected by
    /// this range. The model leaves it empty; the review response fills it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reviewed_events: Vec<SessionMetaforkReviewEvent>,
}

/// The exact model proposal shown to the human before a metafork can commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMetaforkProposal {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removals: Vec<SessionMetaforkRemoval>,
}

/// Complete write-free review manifest shown to the operator.
///
/// The accepted digest covers the operation as well as the removal set, so a
/// reviewed proposal cannot be replayed against a different source, fork
/// coordinate, child name, or directing description.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMetaforkReviewManifest {
    pub command_id: String,
    pub source_session_id: SessionId,
    pub worker_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_branch_id: Option<BranchId>,
    pub fork_node_id: NodeId,
    pub fork_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub description: String,
    pub model_proposal: SessionMetaforkProposal,
}

impl SessionMetaforkReviewManifest {
    /// Content address binding acceptance to the complete reviewed operation.
    pub fn digest(&self) -> Result<String, serde_json::Error> {
        serde_json::to_vec(self).map(|bytes| blake3::hash(&bytes).to_hex().to_string())
    }
}

impl SessionMetaforkProposal {
    /// Content address used to bind human acceptance to the exact proposal.
    pub fn digest(&self) -> Result<String, serde_json::Error> {
        serde_json::to_vec(self).map(|bytes| blake3::hash(&bytes).to_hex().to_string())
    }
}

/// Whether a child copied history verbatim or applied reviewed omissions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionForkMode {
    Fork,
    Metafork,
    #[serde(other)]
    Unknown,
}

/// Physical/model-context boundary policy for the new session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForkContextEpoch {
    /// The child has a new session-scoped cache key and an independent
    /// sidecar root. A first rebuild starts a fresh physical generation at
    /// segment zero and may segment immediately; no parent bytes are inherited.
    Fresh,
    #[serde(other)]
    Unknown,
}

/// One copied envelope whose child prompt rendering was changed to `omit`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHistoryOmission {
    pub source_seq: u64,
    pub child_seq: u64,
    pub source_event_id: EventId,
    pub child_event_id: EventId,
    pub payload_kind: String,
    pub reason: String,
}

/// Prompt-omitted provenance fact appended to every forked child journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionForked {
    pub source_session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_branch_id: Option<BranchId>,
    pub fork_node_id: NodeId,
    pub fork_seq: u64,
    pub mode: SessionForkMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Digest of the complete accepted [`SessionMetaforkReviewManifest`].
    pub accepted_proposal_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub omissions: Vec<SessionHistoryOmission>,
    pub context_epoch: ForkContextEpoch,
}

/// Additive fork-event union kept separate from the exhaustive core event
/// payload enum so older Rust consumers remain source-compatible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionForkEventPayload {
    SessionForked(SessionForked),
    #[serde(other)]
    Unknown,
}

impl SessionForked {
    pub fn to_payload_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(SessionForkEventPayload::SessionForked(self.clone()))
    }

    #[must_use]
    pub fn from_payload_value(value: &serde_json::Value) -> Option<Self> {
        match serde_json::from_value::<SessionForkEventPayload>(value.clone()).ok()? {
            SessionForkEventPayload::SessionForked(record) => Some(record),
            SessionForkEventPayload::Unknown => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SessionForkEventPayload;

    /// MUTATION CHECK: remove `#[serde(other)]` from `Unknown`. Expected
    /// runtime failure: the future additive fact fails to deserialize.
    #[test]
    fn future_session_fork_event_decodes_as_unknown() {
        let payload: SessionForkEventPayload =
            serde_json::from_str(r#"{"type":"future_session_fork_fact"}"#)
                .expect("future fork fact decodes");
        assert!(matches!(payload, SessionForkEventPayload::Unknown));
    }
}
