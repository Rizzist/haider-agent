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
