//! Cross-provider usage report shapes (U1).
//!
//! Additive and tolerant: readers ignore unknown fields, absent optionals
//! decode to defaults, and every field is derived data — account aliases,
//! display identities, normalized meter readings, and journal-derived local
//! counters. Secrets (tokens, API keys, raw JWTs) NEVER appear in these
//! types.

use crate::credential::AuthMethod;
use crate::ids::{CredentialAlias, RunId};
use crate::provider::{CacheStatAvailability, RequestUsage, UsageRequestKind};
use serde::{Deserialize, Serialize};

/// The fixed number of UTC quarter-hour cells in one usage-history day.
pub const USAGE_HISTORY_SLOTS_PER_DAY: usize = 96;

/// The largest heatmap range accepted by the v1 history RPC.
pub const USAGE_HISTORY_MAX_RANGE_DAYS: u16 = 366;

/// Root-vs-delegated accounting lane for one usage-history row.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum UsageHistoryRoleV1 {
    #[default]
    Root,
    Subagent,
    #[serde(other)]
    Unknown,
}

/// Append-only dictionary entry referenced by compact slot rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageHistoryKeyV1 {
    pub id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_family: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<String>,
}

/// One lane's measured counters in a sampled quarter-hour slot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageHistoryRowV1 {
    pub key_id: u32,
    pub role: UsageHistoryRoleV1,
    pub requests: u64,
    pub errors: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
}

/// One sampled quarter-hour cell. A present all-zero row is sampled zero;
/// the enclosing day uses `None` for a cell that was not sampled.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageHistorySlotV1 {
    #[serde(default)]
    pub rows: Vec<UsageHistoryRowV1>,
    #[serde(default)]
    pub subagents_spawned: u64,
}

/// One provider meter reading frozen in the day ledger.
///
/// `basis_points` is the published integer carried into the writer. Readers
/// must not reconstruct it from a normalized floating-point percentage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageHistoryMeterSampleV1 {
    pub account: String,
    pub window: String,
    pub basis_points: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grace_until_ms: Option<u64>,
    pub sampled_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// Provider-published integer credit balance, carried verbatim. This is
    /// a point-in-time balance, not a rate-limit window; absence means the
    /// provider did not publish it and must never be rendered as zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits: Option<i64>,
    /// Provider-published integer held balance, carried verbatim. This is a
    /// point-in-time balance, not a rate-limit window; absence means the
    /// provider did not publish it and must never be rendered as zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale: Option<bool>,
}

/// A daemon-version transition observed within one day file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageHistoryVersionChangeV1 {
    pub daemon_version: String,
    pub changed_at_ms: u64,
}

/// Device-local truth for one UTC day.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageHistoryDayV1 {
    pub date: String,
    pub device_id: String,
    #[serde(default)]
    pub backfilled: bool,
    #[serde(default)]
    pub keys: Vec<UsageHistoryKeyV1>,
    /// Exactly 96 entries. `None` is not-sampled; `Some` may contain zeros.
    pub slots: Vec<Option<UsageHistorySlotV1>>,
    #[serde(default)]
    pub meter_samples: Vec<UsageHistoryMeterSampleV1>,
    #[serde(default)]
    pub version_changes: Vec<UsageHistoryVersionChangeV1>,
}

/// Folded daily counters for the bounded heatmap read. Dollars, rates,
/// sessions, and durations deliberately do not exist in this projection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageHistoryDailyTotalV1 {
    pub sampled_slots: u16,
    pub requests: u64,
    pub errors: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
    pub subagents_spawned: u64,
}

/// One dated heatmap cell. `total=None` is no local sample for that date;
/// a present all-zero total is a measured zero day.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageHistoryRangeDayV1 {
    pub date: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<UsageHistoryDailyTotalV1>,
}

/// Provider-native allowance state from Haider Code's account endpoint.
///
/// Known values may grow over time. Unknown strings are preserved verbatim
/// so an older daemon never turns a future server state into a healthy claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HaiderCodeAllowanceStateV1 {
    Ok,
    Unknown(String),
}

impl Serialize for HaiderCodeAllowanceStateV1 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(match self {
            Self::Ok => "ok",
            Self::Unknown(state) => state,
        })
    }
}

impl<'de> Deserialize<'de> for HaiderCodeAllowanceStateV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let state = String::deserialize(deserializer)?;
        Ok(match state.as_str() {
            "ok" => Self::Ok,
            _ => Self::Unknown(state),
        })
    }
}

/// Weekly allowance fields exactly as Haider Code published them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HaiderCodeWeeklyAllowanceV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent_remaining: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<HaiderCodeAllowanceStateV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grace_until_ms: Option<u64>,
}

/// Account-hold fields exactly as Haider Code published them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HaiderCodeHoldV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_locked: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscribe_banned: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Tolerant snapshot of `GET https://haidercode.ai/v1/account`.
///
/// Every remote field is optional: absence stays absence, and unknown JSON
/// fields are ignored by Serde's default forward-compatible behavior.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HaiderCodePlanSnapshotV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly_allowance: Option<HaiderCodeWeeklyAllowanceV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_credits_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_topup_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold: Option<HaiderCodeHoldV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models_live: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_after_s: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached: Option<bool>,
}

impl HaiderCodePlanSnapshotV1 {
    /// Only explicit provider hold flags halt the account. Missing flags do
    /// not assert either health or failure.
    #[must_use]
    pub fn is_halted(&self) -> bool {
        self.hold.as_ref().is_some_and(|hold| {
            hold.api_locked == Some(true) || hold.subscribe_banned == Some(true)
        })
    }
}

/// Typed result published for the active Haider Code account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
#[non_exhaustive]
pub enum HaiderCodePlanOutcomeV1 {
    Available {
        snapshot: HaiderCodePlanSnapshotV1,
    },
    /// The server reported a snapshot but did not assert a known healthy
    /// allowance state. Clients render its typed/raw fields without guessing.
    Indeterminate {
        snapshot: HaiderCodePlanSnapshotV1,
    },
    Halted {
        snapshot: HaiderCodePlanSnapshotV1,
    },
    Unauthorized,
    #[serde(other)]
    Unknown,
}

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
    /// Hypothetical API-rate equivalent across all known-auth lanes. Kept
    /// separate from `est_cost_usd`, which remains real API-key spend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_equivalent_est_cost_usd: Option<f64>,
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
    /// Logical input attributed to API-key (pay-as-you-go) lanes. The legacy
    /// cost fields below remain the real metered subtotal.
    #[serde(default)]
    pub metered_input_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_with_cache_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_without_cache_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_savings_usd: Option<f64>,
    /// Separately carried API-rate equivalents across every known-auth lane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_equivalent_input_with_cache_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_equivalent_input_without_cache_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_equivalent_estimated_savings_usd: Option<f64>,
    #[serde(default)]
    pub breakdowns: Vec<CacheUsageBreakdownV1>,
    /// Response-local request records, retained even when normalized cache
    /// counters are unavailable. No zero-filled breakdown is synthesized.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requests: Vec<CacheUsageRequestV1>,
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
            && self.metered_input_tokens == 0
            && self.input_with_cache_usd.is_none()
            && self.input_without_cache_usd.is_none()
            && self.estimated_savings_usd.is_none()
            && self.api_equivalent_input_with_cache_usd.is_none()
            && self.api_equivalent_input_without_cache_usd.is_none()
            && self.api_equivalent_estimated_savings_usd.is_none()
            && self.breakdowns.is_empty()
            && self.requests.is_empty()
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
    /// Credential flavor for this lane. Absent is legacy/unknown and is
    /// conservatively treated as not eligible for dollar rendering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_method: Option<AuthMethod>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_equivalent_input_with_cache_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_equivalent_input_without_cache_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_equivalent_estimated_savings_usd: Option<f64>,
}

/// Request-local cache evidence retained without requiring normalized cache
/// counters to exist.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheUsageRequestV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<CacheUsageRequestScopeV1>,
    pub request: RequestUsage,
}

/// Non-secret coordinates for one reported physical request. This omits the
/// legacy plain component digests; the request carries keyed fingerprints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheUsageRequestScopeV1 {
    pub provider: String,
    pub model: String,
    pub cache_epoch: String,
    #[serde(default)]
    pub request_kind: UsageRequestKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_method: Option<AuthMethod>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunId>,
}
