//! Durable announcements of committed prompt compaction overlays.

use crate::history::CompactionResume;
use crate::ids::{ArtifactRef, NodeId};
use serde::{Deserialize, Serialize};

/// The unit of both compaction counts. A provider message can contain several
/// text/tool blocks; it is neither a journal envelope nor a conversation turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionItemUnit {
    ProviderMessage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCompactionV1 {
    /// Durable turn ordinal shared with X-Haider-Turn. Manual idle compaction
    /// owns a separate durable run/turn; automatic compaction uses its trigger.
    pub turn_ordinal: u64,
    /// The successful summarization provider request, including any preceding
    /// rejected replay attempt. This is not the subsequent resumed request.
    pub request_ordinal: u64,
    pub operation_id: String,
    /// Inclusive original tree ancestry range represented by the new summary.
    pub covers_from: NodeId,
    pub covers_to: NodeId,
    /// Actual active provider-message prefix replaced by this summary. On a
    /// second compaction this counts the old summary once, not its historical
    /// source again. Original journal items are retained durably.
    pub dropped_item_count: u64,
    pub dropped_item_unit: CompactionItemUnit,
    /// Original provider-message suffix kept verbatim, excluding the new
    /// summary and request-only system/instruction/volatile-tail messages.
    pub retained_suffix_size: u64,
    pub retained_suffix_unit: CompactionItemUnit,
    pub summary_artifact: ArtifactRef,
    pub resume_cause: CompactionResume,
}

/// Supplemental typed payload preserves source compatibility for exhaustive
/// consumers of the frozen core EventPayload union.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContextCompactionEventPayload {
    ContextCompaction(ContextCompactionV1),
}

impl ContextCompactionEventPayload {
    pub fn to_payload_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(self)
    }

    #[must_use]
    pub fn from_payload_value(value: &serde_json::Value) -> Option<Self> {
        serde_json::from_value(value.clone()).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_compaction_scoped_payload_round_trips_with_explicit_count_units() {
        let payload = serde_json::json!({
            "type": "context_compaction",
            "turn_ordinal": 7,
            "request_ordinal": 3,
            "operation_id": "compact-seven",
            "covers_from": "node-a",
            "covers_to": "node-d",
            "dropped_item_count": 4,
            "dropped_item_unit": "provider_message",
            "retained_suffix_size": 5,
            "retained_suffix_unit": "provider_message",
            "summary_artifact": "blake3:summary-seven",
            "resume_cause": "auto_mid_turn"
        });
        let decoded = ContextCompactionEventPayload::from_payload_value(&payload);
        assert_eq!(
            decoded
                .as_ref()
                .and_then(|value| value.to_payload_value().ok()),
            Some(payload),
            "scope, correlation, and exact units survive journal/JSON replay"
        );
    }
}
