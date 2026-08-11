//! Cross-provider usage report shapes (U1).
//!
//! Additive and tolerant: readers ignore unknown fields, absent optionals
//! decode to defaults, and every field is derived data — account aliases,
//! display identities, normalized meter readings, and journal-derived local
//! counters. Secrets (tokens, API keys, raw JWTs) NEVER appear in these
//! types.

use crate::credential::AuthMethod;
use crate::ids::CredentialAlias;
use crate::provider::{CacheStatAvailability, UsageRequestKind};
use serde::{Deserialize, Serialize};

/// One `usage.report` snapshot: every known account with its meter state and
/// journal-derived local statistics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageReportV1 {
    /// Daemon clock at assembly time (Unix ms).
    pub generated_at_ms: u64,
    #[serde(default)]
    pub accounts: Vec<AccountUsageReportV1>,
}

/// Per-account entry: descriptor coordinates (never secrets), the provider
/// meter state, and locally accounted statistics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountUsageReportV1 {
    pub provider: String,
    pub alias: CredentialAlias,
    /// Human display identity (email/handle) from the credential descriptor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    /// Subscription plan when the provider reports one (e.g. codex
    /// `plan_type`, JWT `chatgpt_plan_type`). Absent when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    pub auth_method: AuthMethod,
    pub meter: AccountMeterStateV1,
    pub local: LocalUsageStatsV1,
}

/// Meter state for one account. `local_only` is an honest "this provider has
/// no server-side meter" — never an error; `unavailable` carries a typed
/// reason and never a fabricated reading.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AccountMeterStateV1 {
    /// Provider-reported windows, normalized (utilization always 0.0–1.0).
    Metered {
        #[serde(default)]
        windows: Vec<UsageWindowV1>,
    },
    /// The meter endpoint exists but this reading failed (auth, HTTP, parse).
    Unavailable { reason: String },
    /// API-key/custom providers: no server meter exists; local counters are
    /// the only truth.
    LocalOnly,
}

/// One provider-reported rate-limit window, normalized.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageWindowV1 {
    /// Provider-native window name (e.g. `primary`, `five_hour`,
    /// `seven_day_sonnet`, `quota`).
    pub window: String,
    /// Fraction used, ALWAYS normalized to 0.0–1.0 regardless of whether the
    /// provider reported a fraction or a percentage.
    pub utilization: f64,
    /// Window reset instant (Unix ms) when the provider reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at_ms: Option<u64>,
    /// Optional provider label for named per-model/per-feature limits (e.g.
    /// codex `additional_rate_limits[].limit_name`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Journal-derived local statistics for one account.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LocalUsageStatsV1 {
    /// Sessions in which this account contributed provider usage.
    pub sessions: u64,
    /// Sum of attributed session spans (first to last committed event), ms.
    pub total_duration_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub reasoning_tokens: u64,
    #[serde(default)]
    pub cached_tokens: u64,
    /// Estimate priced from the bundled static table; absent when no priced
    /// model matched. Never a bill — an estimate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub est_cost_usd: Option<f64>,
    /// Lines added/removed summed from completed fs tool receipts.
    #[serde(default)]
    pub lines_added: u64,
    #[serde(default)]
    pub lines_removed: u64,
    /// Additive cache/pricing detail for the journal-derived counters.
    #[serde(default, skip_serializing_if = "CacheUsageStatsV1::is_empty")]
    pub cache: CacheUsageStatsV1,
}

/// Cache-aware local totals. Costs are input-only because output cost is
/// unchanged by prompt caching.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CacheUsageStatsV1 {
    pub logical_input_tokens: u64,
    pub uncached_input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_write_5m_tokens: u64,
    pub cache_write_1h_tokens: u64,
    pub billed_output_tokens: u64,
    /// Logical input backed by a provider-reported cache split.
    pub telemetry_covered_input_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_with_cache_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_without_cache_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_savings_usd: Option<f64>,
    #[serde(default)]
    pub breakdowns: Vec<CacheUsageBreakdownV1>,
}

impl CacheUsageStatsV1 {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.logical_input_tokens == 0
            && self.uncached_input_tokens == 0
            && self.cache_read_tokens == 0
            && self.cache_write_tokens == 0
            && self.cache_write_5m_tokens == 0
            && self.cache_write_1h_tokens == 0
            && self.billed_output_tokens == 0
            && self.telemetry_covered_input_tokens == 0
            && self.input_with_cache_usd.is_none()
            && self.input_without_cache_usd.is_none()
            && self.estimated_savings_usd.is_none()
            && self.breakdowns.is_empty()
    }
}

/// Provider/model/cache-epoch/request-lane breakdown for `/usage`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CacheUsageBreakdownV1 {
    pub provider: String,
    pub model: String,
    pub cache_epoch: String,
    #[serde(default)]
    pub request_kind: UsageRequestKind,
    pub logical_input_tokens: u64,
    pub uncached_input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_write_5m_tokens: u64,
    pub cache_write_1h_tokens: u64,
    pub billed_output_tokens: u64,
    pub telemetry_covered_input_tokens: u64,
    #[serde(default)]
    pub cache_status: CacheStatAvailability,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_with_cache_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_without_cache_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_savings_usd: Option<f64>,
}
