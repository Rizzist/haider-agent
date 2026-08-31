//! Durable context-occupancy truth, separate from cumulative billing usage.

use crate::item::TurnItem;
use serde::{Deserialize, Serialize};

/// Stable extension-item kind carrying a [`ContextFootprint`] snapshot.
///
/// The extension carrier keeps the event additive for older clients while
/// making the payload typed for clients that advertise context compaction v1.
pub const CONTEXT_FOOTPRINT_EXTENSION_KIND: &str = "context_footprint_v1";
/// Stable extension-item kind carrying one conversation-level context saving.
pub const CONTEXT_SAVINGS_EXTENSION_KIND: &str = "context_savings_v1";

/// Conversation-level compaction tier that changed the provider-bound view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextCompactionTier {
    /// Fast-mode structural trim retaining the newest 24 complete tool pairs.
    StructuralTrim24,
    /// Fast-mode structural trim retaining the newest 12 complete tool pairs.
    StructuralTrim12,
    /// Model-authored replacement summary for the older safe prefix.
    Summarize,
}

/// Honesty marker for the saved-token unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextSavingsMeasurement {
    /// `ceil(serialized provider-request bytes / 4)` before minus after.
    ///
    /// This is deterministic and provider-neutral, but it is not a model
    /// tokenizer and must never be presented as an exact provider token count.
    ProviderRequestBytesDivFourV1,
}

/// One durable operation that reduced the provider-bound conversation view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextSavingsEvent {
    pub tier: ContextCompactionTier,
    pub estimated_tokens_before: u64,
    pub estimated_tokens_after: u64,
    pub estimated_tokens_saved: u64,
    pub session_cumulative_estimated_tokens_saved: u64,
    pub session_operation_count: u64,
    pub measurement: ContextSavingsMeasurement,
    /// Whole tool exchanges removed by a structural tier. Empty for model
    /// summarization and for legacy savings events.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed_tool_call_ids: Vec<String>,
}

impl ContextSavingsEvent {
    /// Wraps this typed event in the protocol's additive item carrier.
    pub fn extension_item(&self) -> Result<TurnItem, serde_json::Error> {
        Ok(TurnItem::Extension {
            kind: CONTEXT_SAVINGS_EXTENSION_KIND.to_owned(),
            data: serde_json::to_value(self)?,
        })
    }

    /// Decodes a typed saving from its additive item carrier.
    pub fn from_extension_item(item: &TurnItem) -> Option<Self> {
        Self::try_from_extension_item(item).ok().flatten()
    }

    /// Decodes a typed saving without hiding malformed payloads for this
    /// known authoritative extension kind.
    pub fn try_from_extension_item(item: &TurnItem) -> Result<Option<Self>, serde_json::Error> {
        let TurnItem::Extension { kind, data } = item else {
            return Ok(None);
        };
        if kind != CONTEXT_SAVINGS_EXTENSION_KIND {
            return Ok(None);
        }
        serde_json::from_value(data.clone()).map(Some)
    }
}

/// Restart-stable context-economy counters stored with typed session metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextEconomy {
    pub cumulative_estimated_tokens_saved: u64,
    pub operation_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event: Option<ContextSavingsEvent>,
}

impl ContextEconomy {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.operation_count == 0
            && self.cumulative_estimated_tokens_saved == 0
            && self.last_event.is_none()
    }

    #[must_use]
    pub fn record(
        &self,
        tier: ContextCompactionTier,
        estimated_tokens_before: u64,
        estimated_tokens_after: u64,
    ) -> (Self, ContextSavingsEvent) {
        self.record_with_removed_tool_calls(
            tier,
            estimated_tokens_before,
            estimated_tokens_after,
            Vec::new(),
        )
    }

    /// Records a structural operation together with the exact provider-view
    /// selection needed to reproduce it after restart.
    #[must_use]
    pub fn record_with_removed_tool_calls(
        &self,
        tier: ContextCompactionTier,
        estimated_tokens_before: u64,
        estimated_tokens_after: u64,
        removed_tool_call_ids: Vec<String>,
    ) -> (Self, ContextSavingsEvent) {
        let estimated_tokens_saved = estimated_tokens_before.saturating_sub(estimated_tokens_after);
        let event = ContextSavingsEvent {
            tier,
            estimated_tokens_before,
            estimated_tokens_after,
            estimated_tokens_saved,
            session_cumulative_estimated_tokens_saved: self
                .cumulative_estimated_tokens_saved
                .saturating_add(estimated_tokens_saved),
            session_operation_count: self.operation_count.saturating_add(1),
            measurement: ContextSavingsMeasurement::ProviderRequestBytesDivFourV1,
            removed_tool_call_ids,
        };
        (
            Self {
                cumulative_estimated_tokens_saved: event.session_cumulative_estimated_tokens_saved,
                operation_count: event.session_operation_count,
                last_event: Some(event.clone()),
            },
            event,
        )
    }
}

/// Complete programmatic context-accounting snapshot for one request boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextAccounting {
    pub used_tokens: u64,
    pub model_limit_tokens: u64,
    pub remaining_tokens: u64,
    /// Integer percentage in basis points (`8_500` = 85.00%).
    pub usage_basis_points: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_tier: Option<ContextCompactionTier>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_tier_at_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_until_next_tier: Option<u64>,
    pub economy: ContextEconomy,
}

/// Honesty marker for one context-footprint snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextFootprintTruth {
    /// Request-local provider usage supplied the token counts.
    Exact,
    /// The outgoing compiled provider request was accounted locally.
    Estimated,
}

/// Tokens occupying one provider context at a request boundary.
///
/// `input_tokens` excludes the cached subset, so `used_tokens` is always the
/// saturating sum of the three displayed splits. This is intentionally not a
/// billing accumulator: every value describes one request-local snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextFootprint {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub used_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    pub reserved_output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub soft_threshold_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_turns_to_threshold: Option<u64>,
    pub truth: ContextFootprintTruth,
    /// Additive complete accounting surface. `None` is emitted by older
    /// producers and by request contexts whose model limit is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accounting: Option<ContextAccounting>,
}

impl ContextFootprint {
    /// Wraps this typed snapshot in the protocol's additive item carrier.
    pub fn extension_item(&self) -> Result<TurnItem, serde_json::Error> {
        Ok(TurnItem::Extension {
            kind: CONTEXT_FOOTPRINT_EXTENSION_KIND.to_owned(),
            data: serde_json::to_value(self)?,
        })
    }

    /// Decodes a typed snapshot from its additive item carrier.
    pub fn from_extension_item(item: &TurnItem) -> Option<Self> {
        let TurnItem::Extension { kind, data } = item else {
            return None;
        };
        (kind == CONTEXT_FOOTPRINT_EXTENSION_KIND)
            .then(|| serde_json::from_value(data.clone()).ok())
            .flatten()
    }
}
