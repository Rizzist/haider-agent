//! Durable context-occupancy truth, separate from cumulative billing usage.

use crate::item::TurnItem;
use serde::de::Error as _;
use serde::{Deserialize, Serialize};

/// Stable extension-item kind carrying a [`ContextFootprint`] snapshot.
///
/// The extension carrier keeps the event additive for older clients while
/// making the payload typed for clients that advertise context compaction v1.
pub const CONTEXT_FOOTPRINT_EXTENSION_KIND: &str = "context_footprint_v1";
/// Stable parent extension kind carrying one conversation-level saving.
pub const CONTEXT_SAVINGS_EXTENSION_KIND: &str = "context_savings_v1";
/// Additive output child of [`CONTEXT_SAVINGS_EXTENSION_KIND`].
///
/// Keeping a distinct kind lets ctx-era readers retain their required `tier`
/// field and safely ignore output records they do not yet understand. Both
/// kinds share one ordered [`ContextEconomy`] ledger and measurement.
pub const CONTEXT_SAVINGS_OUTPUT_EXTENSION_KIND: &str = "context_savings_output_v1";

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

/// Byte-level disclosure for one tool-output projection.
///
/// This is a child of [`ContextSavingsEvent`], not an independently additive
/// counter. Inline `haider_elision_v1` markers repeat only its omission facts
/// so a model can see that output is incomplete without creating a second
/// accounting authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputSavings {
    /// Stable producer-defined scope such as `process_output_adapter`.
    pub scope: String,
    /// Serialized provider-bound text-projection bytes before reduction.
    ///
    /// These are not raw source bytes. JSON string escaping is included so
    /// the token estimate has the same unit as conversation compaction.
    pub input_bytes: u64,
    /// Serialized provider-bound text-projection bytes after reduction.
    pub output_bytes: u64,
    /// Raw source bytes omitted from the model-visible text.
    pub omitted_bytes: u64,
    /// False when a hard/time/enumeration limit proves that more content was
    /// omitted but cannot determine the complete byte count.
    pub omitted_bytes_exact: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub omitted_items_at_least: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub omitted_item_unit: Option<String>,
    /// Stable durable coordinate for exactly-once attribution when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_item_id: Option<String>,
    /// Lower bound on source bytes never observed because execution stopped at
    /// a hard/time bound. This is disclosure, not additional saved input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_omitted_bytes_at_least: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retained_head_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retained_tail_bytes: Option<u64>,
    pub estimated_tokens_before: u64,
    pub estimated_tokens_after: u64,
    /// Signed net after marker overhead. Negative means the projection grew.
    pub estimated_net_tokens_saved: i64,
    pub estimated_tokens_saved: u64,
    pub measurement: ContextSavingsMeasurement,
}

impl OutputSavings {
    #[must_use]
    pub fn from_provider_request_bytes(
        scope: impl Into<String>,
        input_bytes: usize,
        output_bytes: usize,
        omitted_bytes: usize,
        omitted_bytes_exact: bool,
    ) -> Self {
        let input_bytes = u64::try_from(input_bytes).unwrap_or(u64::MAX);
        let output_bytes = u64::try_from(output_bytes).unwrap_or(u64::MAX);
        let estimated_tokens_before = input_bytes.saturating_add(3) / 4;
        let estimated_tokens_after = output_bytes.saturating_add(3) / 4;
        Self {
            scope: scope.into(),
            input_bytes,
            output_bytes,
            omitted_bytes: u64::try_from(omitted_bytes).unwrap_or(u64::MAX),
            omitted_bytes_exact,
            omitted_items_at_least: None,
            omitted_item_unit: None,
            source_item_id: None,
            source_omitted_bytes_at_least: None,
            retained_head_bytes: None,
            retained_tail_bytes: None,
            estimated_tokens_before,
            estimated_tokens_after,
            estimated_net_tokens_saved: signed_difference(
                estimated_tokens_before,
                estimated_tokens_after,
            ),
            estimated_tokens_saved: estimated_tokens_before.saturating_sub(estimated_tokens_after),
            measurement: ContextSavingsMeasurement::ProviderRequestBytesDivFourV1,
        }
    }

    fn is_coherent(&self) -> bool {
        let expected_before = self.input_bytes.saturating_add(3) / 4;
        let expected_after = self.output_bytes.saturating_add(3) / 4;
        self.estimated_tokens_before == expected_before
            && self.estimated_tokens_after == expected_after
            && self.estimated_net_tokens_saved == signed_difference(expected_before, expected_after)
            && self.estimated_tokens_saved == expected_before.saturating_sub(expected_after)
            && self.measurement == ContextSavingsMeasurement::ProviderRequestBytesDivFourV1
    }
}

/// Serialized byte length of one provider-bound text projection.
///
/// A tool result is eventually carried as a JSON string in the neutral
/// request projection. Counting the encoded string, rather than raw UTF-8,
/// keeps quotes, backslashes, and control characters in the same accounting
/// unit used by conversation compaction.
#[must_use]
pub fn provider_request_text_projection_bytes(text: &str) -> usize {
    serde_json::to_vec(text).map_or(usize::MAX, |encoded| encoded.len())
}

fn signed_difference(before: u64, after: u64) -> i64 {
    if before >= after {
        i64::try_from(before - after).unwrap_or(i64::MAX)
    } else {
        -i64::try_from(after - before).unwrap_or(i64::MAX)
    }
}

/// Layer that owns one saving. Conversation is the default so ctx-era events
/// written before output accounting remain decodable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextSavingsLayer {
    #[default]
    Conversation,
    ToolOutput,
}

/// One durable operation that reduced the provider-bound view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextSavingsEvent {
    #[serde(default)]
    pub layer: ContextSavingsLayer,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<ContextCompactionTier>,
    /// Whole tool exchanges removed by a structural tier. Empty for summary
    /// and output-level events.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed_tool_call_ids: Vec<String>,
    /// Child disclosure for an output-level event. It is absent from
    /// conversation-level events and is never a second additive record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<OutputSavings>,
    pub estimated_tokens_before: u64,
    pub estimated_tokens_after: u64,
    pub estimated_tokens_saved: u64,
    pub session_cumulative_estimated_tokens_saved: u64,
    pub session_operation_count: u64,
    pub measurement: ContextSavingsMeasurement,
}

impl ContextSavingsEvent {
    /// Wraps this typed event in the protocol's additive item carrier.
    pub fn extension_item(&self) -> Result<TurnItem, serde_json::Error> {
        let kind = match self.layer {
            ContextSavingsLayer::Conversation => CONTEXT_SAVINGS_EXTENSION_KIND,
            ContextSavingsLayer::ToolOutput => CONTEXT_SAVINGS_OUTPUT_EXTENSION_KIND,
        };
        Ok(TurnItem::Extension {
            kind: kind.to_owned(),
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
        if kind != CONTEXT_SAVINGS_EXTENSION_KIND && kind != CONTEXT_SAVINGS_OUTPUT_EXTENSION_KIND {
            return Ok(None);
        }
        let event: Self = serde_json::from_value(data.clone())?;
        let kind_matches_layer = matches!(
            (kind.as_str(), event.layer),
            (
                CONTEXT_SAVINGS_EXTENSION_KIND,
                ContextSavingsLayer::Conversation
            ) | (
                CONTEXT_SAVINGS_OUTPUT_EXTENSION_KIND,
                ContextSavingsLayer::ToolOutput
            )
        );
        if !kind_matches_layer || !event.is_coherent() {
            return Err(serde_json::Error::custom(
                "incoherent context-savings payload",
            ));
        }
        Ok(Some(event))
    }

    #[must_use]
    pub fn conversation(&self) -> Option<(ContextCompactionTier, &[String])> {
        (self.layer == ContextSavingsLayer::Conversation && self.output.is_none())
            .then_some((self.tier?, self.removed_tool_call_ids.as_slice()))
    }

    #[must_use]
    pub fn tool_output(&self) -> Option<&OutputSavings> {
        (self.layer == ContextSavingsLayer::ToolOutput
            && self.tier.is_none()
            && self.removed_tool_call_ids.is_empty())
        .then_some(self.output.as_ref())
        .flatten()
    }

    fn is_coherent(&self) -> bool {
        let layer_is_coherent = match self.layer {
            ContextSavingsLayer::Conversation => self.conversation().is_some(),
            ContextSavingsLayer::ToolOutput => {
                self.tool_output().is_some_and(OutputSavings::is_coherent)
            }
        };
        let output_coordinates_match = self.output.as_ref().is_none_or(|output| {
            output.estimated_tokens_before == self.estimated_tokens_before
                && output.estimated_tokens_after == self.estimated_tokens_after
                && output.estimated_tokens_saved == self.estimated_tokens_saved
                && output.measurement == self.measurement
        });
        layer_is_coherent
            && output_coordinates_match
            && self.estimated_tokens_saved
                == self
                    .estimated_tokens_before
                    .saturating_sub(self.estimated_tokens_after)
            && self.measurement == ContextSavingsMeasurement::ProviderRequestBytesDivFourV1
    }
}

/// Restart-stable context-economy counters stored with typed session metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextEconomy {
    pub cumulative_estimated_tokens_saved: u64,
    pub operation_count: u64,
    /// Newest conversation-level event. This deliberately retains the ctx-v1
    /// shape so older typed readers never encounter an absent required tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event: Option<ContextSavingsEvent>,
    /// Newest additive output child. Older ctx-v1 readers ignore this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_output_event: Option<ContextSavingsEvent>,
}

impl ContextEconomy {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.operation_count == 0
            && self.cumulative_estimated_tokens_saved == 0
            && self.last_event.is_none()
            && self.last_output_event.is_none()
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
        self.record_operation(
            ContextSavingsLayer::Conversation,
            Some(tier),
            removed_tool_call_ids,
            None,
            estimated_tokens_before,
            estimated_tokens_after,
        )
    }

    /// Records one output-level reduction in the same cumulative ledger as
    /// conversation compaction. The output detail uses the same estimator.
    #[must_use]
    pub fn record_tool_output(&self, output: OutputSavings) -> (Self, ContextSavingsEvent) {
        let estimated_tokens_before = output.estimated_tokens_before;
        let estimated_tokens_after = output.estimated_tokens_after;
        self.record_operation(
            ContextSavingsLayer::ToolOutput,
            None,
            Vec::new(),
            Some(output),
            estimated_tokens_before,
            estimated_tokens_after,
        )
    }

    fn record_operation(
        &self,
        layer: ContextSavingsLayer,
        tier: Option<ContextCompactionTier>,
        removed_tool_call_ids: Vec<String>,
        output: Option<OutputSavings>,
        estimated_tokens_before: u64,
        estimated_tokens_after: u64,
    ) -> (Self, ContextSavingsEvent) {
        let estimated_tokens_saved = estimated_tokens_before.saturating_sub(estimated_tokens_after);
        let event = ContextSavingsEvent {
            layer,
            tier,
            removed_tool_call_ids,
            output,
            estimated_tokens_before,
            estimated_tokens_after,
            estimated_tokens_saved,
            session_cumulative_estimated_tokens_saved: self
                .cumulative_estimated_tokens_saved
                .saturating_add(estimated_tokens_saved),
            session_operation_count: self.operation_count.saturating_add(1),
            measurement: ContextSavingsMeasurement::ProviderRequestBytesDivFourV1,
        };
        let (last_event, last_output_event) = match layer {
            ContextSavingsLayer::Conversation => {
                (Some(event.clone()), self.last_output_event.clone())
            }
            ContextSavingsLayer::ToolOutput => (self.last_event.clone(), Some(event.clone())),
        };
        (
            Self {
                cumulative_estimated_tokens_saved: event.session_cumulative_estimated_tokens_saved,
                operation_count: event.session_operation_count,
                last_event,
                last_output_event,
            },
            event,
        )
    }
}

/// One deterministic model-visible text elision and its child accounting data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElidedText {
    pub text: String,
    pub savings: OutputSavings,
}

#[derive(Debug, Clone, Copy)]
struct TextProjectionMeasure {
    source_bytes: usize,
    provider_request_bytes: usize,
}

impl TextProjectionMeasure {
    fn complete(text: &str) -> Self {
        Self {
            source_bytes: text.len(),
            provider_request_bytes: provider_request_text_projection_bytes(text),
        }
    }

    fn with_omitted_bytes(selected: &str, omitted_bytes_at_least: usize) -> Self {
        Self {
            source_bytes: selected.len().saturating_add(omitted_bytes_at_least),
            provider_request_bytes: provider_request_text_projection_bytes(selected)
                .saturating_add(omitted_bytes_at_least),
        }
    }
}

/// Keeps one quarter of the available content budget from the head and three
/// quarters from the tail. The head identifies the source; the larger tail
/// preserves diagnostics, summaries, and causal endings.
#[must_use]
pub fn elide_text_head_tail(input: &str, max_bytes: usize, scope: &str) -> Option<ElidedText> {
    if input.len() <= max_bytes {
        return None;
    }
    Some(account_text_elision(
        TextProjectionMeasure::complete(input),
        input,
        max_bytes,
        scope,
        true,
        true,
        None,
    ))
}

/// Marks content that an upstream hard/time/enumeration bound already
/// omitted. `omitted_bytes_at_least` may be zero when stopping the producer
/// proves incompleteness but no unread byte was observed.
#[must_use]
pub fn mark_text_elision(
    selected: &str,
    max_bytes: usize,
    scope: &str,
    omitted_bytes_at_least: usize,
    omitted_bytes_exact: bool,
) -> ElidedText {
    account_text_elision(
        TextProjectionMeasure::with_omitted_bytes(selected, omitted_bytes_at_least),
        selected,
        max_bytes,
        scope,
        omitted_bytes_exact,
        true,
        None,
    )
}

/// Marks an upstream enumeration/depth bound whose omitted byte size is
/// unknown while retaining an honest lower-bound item count.
#[must_use]
pub fn mark_text_elision_with_items(
    selected: &str,
    max_bytes: usize,
    scope: &str,
    omitted_bytes_at_least: usize,
    omitted_bytes_exact: bool,
    omitted_items_at_least: u64,
    omitted_item_unit: &str,
) -> ElidedText {
    account_text_elision(
        TextProjectionMeasure::with_omitted_bytes(selected, omitted_bytes_at_least),
        selected,
        max_bytes,
        scope,
        omitted_bytes_exact,
        true,
        Some((omitted_items_at_least, omitted_item_unit)),
    )
}

fn account_text_elision(
    input: TextProjectionMeasure,
    selected: &str,
    max_bytes: usize,
    scope: &str,
    omitted_bytes_exact: bool,
    split_selected: bool,
    omitted_items: Option<(u64, &str)>,
) -> ElidedText {
    let mut savings = OutputSavings::from_provider_request_bytes(
        scope,
        input.provider_request_bytes,
        provider_request_text_projection_bytes(selected),
        input.source_bytes.saturating_sub(selected.len()),
        omitted_bytes_exact,
    );
    if let Some((count, unit)) = omitted_items {
        savings.omitted_items_at_least = Some(count);
        savings.omitted_item_unit = Some(unit.to_owned());
    }
    let mut result = String::new();
    for _ in 0..32 {
        let marker = text_elision_marker(&savings);
        let content_budget = max_bytes.saturating_sub(marker.len());
        let selected_budget = content_budget.min(selected.len());
        let head_budget = if split_selected || selected.len() > content_budget {
            selected_budget / 4
        } else {
            selected_budget
        };
        let head = utf8_prefix(selected, head_budget);
        let tail_budget = selected_budget.saturating_sub(head.len());
        let tail_start = utf8_suffix_start(selected, tail_budget).max(head.len());
        let tail = &selected[tail_start..];
        let retained = head.len().saturating_add(tail.len());
        result.clear();
        result.push_str(head);
        result.push_str(&marker);
        result.push_str(tail);
        let mut next = OutputSavings::from_provider_request_bytes(
            scope,
            input.provider_request_bytes,
            provider_request_text_projection_bytes(&result),
            input.source_bytes.saturating_sub(retained),
            omitted_bytes_exact,
        );
        if let Some((count, unit)) = omitted_items {
            next.omitted_items_at_least = Some(count);
            next.omitted_item_unit = Some(unit.to_owned());
        }
        next.retained_head_bytes = Some(u64::try_from(head.len()).unwrap_or(u64::MAX));
        next.retained_tail_bytes = Some(u64::try_from(tail.len()).unwrap_or(u64::MAX));
        let stable = next == savings;
        savings = next;
        if stable {
            break;
        }
    }
    let marker = text_elision_marker(&savings);
    let content_budget = max_bytes.saturating_sub(marker.len());
    let selected_budget = content_budget.min(selected.len());
    let head_budget = if split_selected || selected.len() > content_budget {
        selected_budget / 4
    } else {
        selected_budget
    };
    let head = utf8_prefix(selected, head_budget);
    let tail_start =
        utf8_suffix_start(selected, selected_budget.saturating_sub(head.len())).max(head.len());
    let tail = &selected[tail_start..];
    result.clear();
    result.push_str(head);
    result.push_str(&marker);
    result.push_str(tail);
    ElidedText {
        text: result,
        savings,
    }
}

fn text_elision_marker(savings: &OutputSavings) -> String {
    let payload = serde_json::json!({
        "haider_elision_v1": {
            "scope": savings.scope,
            "omitted_bytes": savings.omitted_bytes,
            "omitted_bytes_exact": savings.omitted_bytes_exact,
            "omitted_items_at_least": savings.omitted_items_at_least,
            "omitted_item_unit": savings.omitted_item_unit,
            "retained_head_bytes": savings.retained_head_bytes,
            "retained_tail_bytes": savings.retained_tail_bytes,
        }
    });
    format!("\n{payload}\n")
}

fn utf8_prefix(input: &str, max_bytes: usize) -> &str {
    let mut end = input.len().min(max_bytes);
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    &input[..end]
}

fn utf8_suffix_start(input: &str, max_bytes: usize) -> usize {
    let mut start = input.len().saturating_sub(max_bytes);
    while start < input.len() && !input.is_char_boundary(start) {
        start += 1;
    }
    start
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

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    #[test]
    fn output_and_conversation_share_one_additive_estimated_stream() {
        let mut output =
            OutputSavings::from_provider_request_bytes("fixture", 4_001, 401, 3_600, true);
        output.retained_head_bytes = Some(100);
        output.retained_tail_bytes = Some(301);
        assert_eq!(output.estimated_tokens_before, 1_001);
        assert_eq!(output.estimated_tokens_after, 101);
        assert_eq!(output.estimated_net_tokens_saved, 900);
        assert_eq!(output.estimated_tokens_saved, 900);
        assert_eq!(
            output.measurement,
            ContextSavingsMeasurement::ProviderRequestBytesDivFourV1
        );

        let (economy, event) = ContextEconomy::default().record_tool_output(output.clone());
        let item = event.extension_item().expect("typed extension serializes");
        assert_eq!(ContextSavingsEvent::from_extension_item(&item), Some(event));
        let TurnItem::Extension { kind, data } = item else {
            panic!("context savings uses the forward-compatible extension carrier");
        };
        assert_eq!(kind, CONTEXT_SAVINGS_OUTPUT_EXTENSION_KIND);
        assert_eq!(economy.cumulative_estimated_tokens_saved, 900);
        assert!(data.get("estimated_tokens_saved").is_some());
        assert!(data.get("saved_tokens").is_none());
        assert_eq!(
            data.get("layer").and_then(serde_json::Value::as_str),
            Some("tool_output")
        );
    }

    #[test]
    fn marker_overhead_is_preserved_as_a_negative_signed_net() {
        let output = OutputSavings::from_provider_request_bytes("small-marker", 4, 40, 0, true);
        assert_eq!(output.estimated_tokens_before, 1);
        assert_eq!(output.estimated_tokens_after, 10);
        assert_eq!(output.estimated_net_tokens_saved, -9);
        assert_eq!(output.estimated_tokens_saved, 0);
    }

    #[test]
    fn authoritative_output_event_rejects_disagreeing_child_coordinates() {
        let output = OutputSavings::from_provider_request_bytes("fixture", 4_000, 400, 3_600, true);
        let (_, event) = ContextEconomy::default().record_tool_output(output);
        let mut item = event.extension_item().expect("typed extension serializes");
        let TurnItem::Extension { data, .. } = &mut item else {
            panic!("savings event uses extension carrier")
        };
        data["output"]["estimated_tokens_after"] = serde_json::json!(999);
        assert!(ContextSavingsEvent::try_from_extension_item(&item).is_err());
    }
}
