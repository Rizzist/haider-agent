//! Durable context-occupancy truth, separate from cumulative billing usage.

use crate::item::TurnItem;
use serde::{Deserialize, Serialize};

/// Stable extension-item kind carrying a [`ContextFootprint`] snapshot.
///
/// The extension carrier keeps the event additive for older clients while
/// making the payload typed for clients that advertise context compaction v1.
pub const CONTEXT_FOOTPRINT_EXTENSION_KIND: &str = "context_footprint_v1";

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
