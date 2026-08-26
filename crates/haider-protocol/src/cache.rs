//! Cache-epoch policy and visible transition facts (CM3).

use crate::item::TurnItem;
use crate::provider::CacheRequestDiagnosticV1;
use serde::{Deserialize, Serialize};

/// Stable additive extension kind for a named cache-epoch transition.
pub const CACHE_EPOCH_TRANSITION_EXTENSION_KIND: &str = "cache_epoch_transition_v1";

/// Stable hidden extension kind written immediately before a physical
/// provider request. It preserves hashes even when opening or streaming the
/// request fails before the provider can report usage.
pub const CACHE_REQUEST_ATTEMPT_EXTENSION_KIND: &str = "cache_request_attempt_v1";

/// Stable hidden extension kind for the exact, provider-rendered cacheable
/// view written immediately before a physical provider request.
///
/// Unlike [`CACHE_REQUEST_ATTEMPT_EXTENSION_KIND`], this record deliberately
/// contains exact serialized prompt bytes. It is conversation-store state,
/// not telemetry: restart/resume validation must be able to byte-compare the
/// old provider prefix instead of trusting a digest produced by new code.
pub const PROVIDER_VIEW_ATTEMPT_EXTENSION_KIND: &str = "provider_view_attempt_v1";

/// One explicit provider cache boundary selected by the placement planner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderViewBoundaryV1 {
    /// Stable section name (`system`, `tools`, or `history`).
    pub section: String,
    /// Exclusive normalized-message boundary for history markers. System and
    /// tool markers omit it because they are not conversation messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_end: Option<u64>,
}

/// Exact immutable provider view associated with one cacheable request.
///
/// The three byte sections are serialized by the selected adapter using its
/// declared `serialization_version`. History is stored block-by-block so an
/// append-only reconstruction can compare the exact old prefix without
/// re-encoding durable data. The volatile newest tail is intentionally not
/// included: it is never eligible for a cache marker or this invariant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderViewLedgerV1 {
    pub provider: String,
    pub model: String,
    pub dialect: String,
    pub serialization_version: String,
    /// Content address of provider/model/system/tools/dialect/serialization.
    pub header_epoch: String,
    /// Full request cache domain, including auth/reasoning/compaction state.
    pub cache_epoch: String,
    pub compaction_epoch: String,
    /// The retention rule that shaped provider-owned reasoning blocks.
    pub reasoning_retention: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_scope: Option<String>,
    pub stable_history_end: u64,
    pub current_user_start: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_compaction_summary_end: Option<u64>,
    /// Explicit epoch-changing trim sentinel. Root histories retain the root
    /// compaction epoch here rather than inventing a rotating trim window.
    pub trim_sentinel: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub boundaries: Vec<ProviderViewBoundaryV1>,
    pub system_bytes: Vec<u8>,
    pub tool_schema_bytes: Vec<u8>,
    pub history_blocks: Vec<Vec<u8>>,
}

/// Dispatch-time wrapper which orders exact views alongside request usage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderViewAttemptV1 {
    pub ordinal: u64,
    pub view: ProviderViewLedgerV1,
}

impl ProviderViewAttemptV1 {
    pub fn extension_item(&self) -> Result<TurnItem, serde_json::Error> {
        Ok(TurnItem::Extension {
            kind: PROVIDER_VIEW_ATTEMPT_EXTENSION_KIND.to_owned(),
            data: serde_json::to_value(self)?,
        })
    }

    /// Strict parser for the conversation store. A malformed known record is
    /// corruption and must not silently downgrade exact-prefix validation.
    pub fn try_from_extension_item(item: &TurnItem) -> Result<Option<Self>, serde_json::Error> {
        let TurnItem::Extension { kind, data } = item else {
            return Ok(None);
        };
        if kind != PROVIDER_VIEW_ATTEMPT_EXTENSION_KIND {
            return Ok(None);
        }
        serde_json::from_value(data.clone()).map(Some)
    }
}

/// Hashes-and-counts-only evidence captured at provider dispatch time.
/// Response-local counters later join this record by `ordinal` through
/// [`crate::provider::RequestUsage`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheRequestAttemptV1 {
    pub ordinal: u64,
    pub diagnostic: CacheRequestDiagnosticV1,
}

impl CacheRequestAttemptV1 {
    pub fn extension_item(&self) -> Result<TurnItem, serde_json::Error> {
        Ok(TurnItem::Extension {
            kind: CACHE_REQUEST_ATTEMPT_EXTENSION_KIND.to_owned(),
            data: serde_json::to_value(self)?,
        })
    }

    #[must_use]
    pub fn from_extension_item(item: &TurnItem) -> Option<Self> {
        let TurnItem::Extension { kind, data } = item else {
            return None;
        };
        (kind == CACHE_REQUEST_ATTEMPT_EXTENSION_KIND)
            .then(|| serde_json::from_value(data.clone()).ok())
            .flatten()
    }
}

/// Session policy for cache-destructive configuration changes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CachePolicyMode {
    Economy,
    #[default]
    Balanced,
    Mobility,
}

/// Durable session cache policy. The balanced threshold is configurable and
/// defaults conservatively; callers may override it at session creation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachePolicySettingsV1 {
    #[serde(default)]
    pub mode: CachePolicyMode,
    #[serde(default = "default_cold_cost_threshold_microusd")]
    pub cold_cost_threshold_microusd: u64,
}

pub const fn default_cold_cost_threshold_microusd() -> u64 {
    50_000
}

impl Default for CachePolicySettingsV1 {
    fn default() -> Self {
        Self {
            mode: CachePolicyMode::Balanced,
            cold_cost_threshold_microusd: default_cold_cost_threshold_microusd(),
        }
    }
}

impl CachePolicySettingsV1 {
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Named reason for a deliberate cache-domain change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheEpochTransitionReason {
    ConfigurationChanged,
    InstructionsChanged,
    ToolPackChanged,
    SystemVersionChanged,
    WebToolDegradation,
    Compaction,
}

/// Durable, UI-visible explanation for a cold boundary. It is operational
/// metadata only and must always ride `PromptRender::Omit`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheEpochTransitionV1 {
    pub reason: CacheEpochTransitionReason,
    /// Compaction is planned lifecycle work, never a failure/miss diagnosis.
    #[serde(default)]
    pub planned: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_fields: Vec<String>,
    #[serde(default)]
    pub invalidated_stable_tokens: u64,
    /// Present only for an API-key lane with known registry pricing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rewarm_cost_usd: Option<f64>,
    /// Registry-derived base-input equivalents, useful even when a plan lane
    /// intentionally omits dollars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rewarm_base_input_equivalent_tokens: Option<f64>,
    /// Stable identity for deduplicating a named component transition.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub transition_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_cache_epoch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_cache_epoch: Option<String>,
}

impl CacheEpochTransitionV1 {
    pub fn extension_item(&self) -> Result<TurnItem, serde_json::Error> {
        Ok(TurnItem::Extension {
            kind: CACHE_EPOCH_TRANSITION_EXTENSION_KIND.to_owned(),
            data: serde_json::to_value(self)?,
        })
    }

    #[must_use]
    pub fn from_extension_item(item: &TurnItem) -> Option<Self> {
        let TurnItem::Extension { kind, data } = item else {
            return None;
        };
        (kind == CACHE_EPOCH_TRANSITION_EXTENSION_KIND)
            .then(|| serde_json::from_value(data.clone()).ok())
            .flatten()
    }

    #[must_use]
    pub fn display_label(&self) -> String {
        let base = match self.reason {
            CacheEpochTransitionReason::ConfigurationChanged => "configuration changed",
            CacheEpochTransitionReason::InstructionsChanged => "instructions changed",
            CacheEpochTransitionReason::ToolPackChanged => "tool pack changed",
            CacheEpochTransitionReason::SystemVersionChanged => "system version changed",
            CacheEpochTransitionReason::WebToolDegradation => "web tool degraded",
            CacheEpochTransitionReason::Compaction => {
                "planned cache epoch transition; next turn history cold"
            }
        };
        if self.reason == CacheEpochTransitionReason::Compaction {
            return format!("· {base}");
        }
        let fields = if self.changed_fields.is_empty() {
            String::new()
        } else {
            format!(" ({})", self.changed_fields.join(", "))
        };
        let mut label = format!(
            "· {base}{fields}; next turn cold — {} stable tokens invalidated",
            self.invalidated_stable_tokens
        );
        if let Some(cost) = self.rewarm_cost_usd {
            label.push_str(&format!(" · est ${cost:.4} re-warm"));
        } else if self.rewarm_base_input_equivalent_tokens.is_some() {
            label.push_str(" · plan");
        }
        label
    }
}
