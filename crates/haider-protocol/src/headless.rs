//! Durable headless-run policy, budget, and reproducibility facts.
//!
//! These values are part of the ordinary event contract. A headless client
//! therefore consumes the same typed journal as every other surface instead
//! of maintaining a second output-only schema.

use crate::ids::RunId;
use crate::session::SessionPermissionOverridesV1;
use serde::{Deserialize, Serialize};

fn permissions_are_empty(value: &SessionPermissionOverridesV1) -> bool {
    value.is_empty()
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Daemon-enforced limits for one accepted run. An absent field is unbounded;
/// zero is rejected at admission rather than being interpreted as absence.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunBudgetV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// Maximum estimated provider cost in millionths of one US dollar.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_microusd: Option<u64>,
    /// Absolute run lifetime measured from durable acceptance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_time_ms: Option<u64>,
}

impl RunBudgetV1 {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.max_tokens.is_none() && self.max_cost_microusd.is_none() && self.max_time_ms.is_none()
    }
}

/// Fully resolved execution settings pinned to an accepted headless run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeadlessRunSpecV1 {
    /// Canonical workspace used to create the autonomous session.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cwd: String,
    pub provider: String,
    pub model: String,
    pub max_output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default)]
    pub fast: bool,
    /// Operator-requested reproducibility seed. The v1 provider transport has
    /// no portable seed field, so this is an audit/replay pin rather than a
    /// claim that a provider applied it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
    #[serde(default, skip_serializing_if = "permissions_are_empty")]
    pub permission_overrides: SessionPermissionOverridesV1,
    #[serde(default, skip_serializing_if = "is_false")]
    pub trust_hooks: bool,
    #[serde(default, skip_serializing_if = "RunBudgetV1::is_empty")]
    pub budget: RunBudgetV1,
    /// Absolute client/run deadline. Unlike `budget.max_time_ms`, which starts
    /// at durable acceptance, this preserves time already spent connecting
    /// and configuring a `haider run --timeout` request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_deadline_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_of: Option<RunId>,
}

/// The exact limit which ended a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunBudgetDimensionV1 {
    Tokens,
    Cost,
    Time,
    /// Forward-compatible value for dimensions introduced by a newer peer.
    #[serde(other)]
    Unknown,
}

/// Canonical run-local usage used by both enforcement and structured output.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeadlessRunUsageV1 {
    pub logical_input_tokens: u64,
    pub billed_output_tokens: u64,
    pub additional_reasoning_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub total_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost_microusd: Option<u64>,
    pub elapsed_ms: u64,
}

/// Structured terminal cause committed before `RunFailed`/`Errored`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunBudgetExhaustedV1 {
    pub dimension: RunBudgetDimensionV1,
    pub limit: u64,
    pub usage: HeadlessRunUsageV1,
}

/// Durable cause for an accepted run whose absolute caller deadline elapsed.
///
/// This is distinct from [`RunBudgetExhaustedV1`]: `--timeout` includes time
/// spent before durable acceptance, while `budget.max_time_ms` starts at the
/// acceptance fact and retains the budget-exhaustion exit contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunDeadlineExceededV1 {
    pub deadline_unix_ms: u64,
}

/// Semantic comparison between a source run and its re-execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayDivergenceV1 {
    pub source_run_id: RunId,
    pub replay_run_id: RunId,
    pub final_text_matches: bool,
    pub tool_trace_matches: bool,
    pub usage_matches: bool,
    pub terminal_matches: bool,
    pub diverged: bool,
}

/// Additive headless-run journal facts. This union is decoded from raw
/// envelopes so the frozen core [`crate::EventPayload`] enum remains source
/// compatible for exhaustive consumers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HeadlessRunEventPayload {
    HeadlessRunConfigured(HeadlessRunSpecV1),
    RunBudgetExhausted(RunBudgetExhaustedV1),
    RunDeadlineExceeded(RunDeadlineExceededV1),
}

impl HeadlessRunEventPayload {
    pub fn to_payload_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(self)
    }

    #[must_use]
    pub fn from_payload_value(value: &serde_json::Value) -> Option<Self> {
        serde_json::from_value(value.clone()).ok()
    }
}
