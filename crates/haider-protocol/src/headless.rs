//! Durable headless-run policy, budget, and reproducibility facts.
//!
//! These values are part of the ordinary event contract. A headless client
//! therefore consumes the same typed journal as every other surface instead
//! of maintaining a second output-only schema.

use crate::error::ErrorCode;
use crate::ids::RunId;
use crate::session::SessionPermissionOverridesV1;
use crate::state::RunState;
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
    /// Logical provider requests per turn; transport retries do not consume
    /// this budget. Absence selects the ordinary 32/64 request policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_budget: Option<crate::request_budget::RequestBudgetV1>,
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
        self.request_budget.is_none() && !self.has_shared_limits()
    }

    /// Token, cost, and time limits use the shared run/child coordinator.
    /// Request tranches are enforced independently by each turn's actor and
    /// must not create a polling usage monitor when these limits are absent.
    #[must_use]
    pub const fn has_shared_limits(&self) -> bool {
        self.max_tokens.is_some() || self.max_cost_microusd.is_some() || self.max_time_ms.is_some()
    }
}

/// Fully resolved execution settings pinned to an accepted headless run.
/// One operator-authored delegation, executed without a parent model request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSpawnSpecV1 {
    pub task: String,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_trigger: Option<String>,
}

/// Fully resolved execution settings pinned to an accepted headless run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeadlessRunSpecV1 {
    /// Requires `agent_cli_v1`; omission preserves the ordinary model turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_spawn: Option<AgentSpawnSpecV1>,
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
    /// Explicit same-session continuation of the latest terminal budget
    /// checkpoint. Admission consumes this handle atomically with the new turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation_of: Option<RunId>,
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

/// Why a run-budget decision refused or stopped provider work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunBudgetDecisionReasonV1 {
    ActualUsage,
    TimeElapsed,
    ProjectedRequest,
    PricingUnavailable {
        provider: String,
        model: String,
    },
    UsageUnavailable {
        provider: String,
        model: String,
    },
    /// Forward-compatible value for reasons introduced by a newer peer.
    #[serde(other)]
    Unknown,
}

/// The values used to make one observable run-budget decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunBudgetDecisionV1 {
    /// Run usage already committed in the selected dimension.
    pub spent: u64,
    /// Projected incremental usage for the candidate request, or absent when
    /// the projection is unavailable or does not apply to this decision.
    #[serde(default)]
    pub projected: Option<u64>,
    pub cap: u64,
    pub reason: RunBudgetDecisionReasonV1,
}

/// Structured terminal cause committed before `RunFailed`/`Errored`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunBudgetExhaustedV1 {
    pub dimension: RunBudgetDimensionV1,
    pub limit: u64,
    pub usage: HeadlessRunUsageV1,
    /// Additive decision detail. Absent on events written by older daemons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<RunBudgetDecisionV1>,
}

impl RunBudgetExhaustedV1 {
    /// Stable human summary accompanying the typed budget fact.
    #[must_use]
    pub fn summary(&self) -> String {
        let Some(decision) = self.decision.as_ref() else {
            return format!(
                "headless {:?} budget exhausted at limit {}",
                self.dimension, self.limit
            );
        };
        let projected = decision
            .projected
            .map_or_else(|| "unavailable".to_owned(), |value| value.to_string());
        format!(
            "headless {:?} budget exhausted: spent {}, projected {}, cap {}, reason {:?}",
            self.dimension, decision.spent, projected, decision.cap, decision.reason
        )
    }
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

/// Additive fields carried by the one durable terminal `run_state` envelope.
///
/// This projection is shared by journal writers and compatibility readers so
/// a terminal has one stable payload shape on live and replay surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurableRunTerminalV1 {
    pub terminal_kind: &'static str,
    pub error_code: Option<&'static str>,
}

/// Classifies the durable terminal payload from facts in the same run.
///
/// Deadline and first blocking facts take precedence over the generic
/// `RunFailed` code because they are the durable cause records for those
/// terminal kinds. Callers pass mutually exclusive deadline/blocking facts by
/// preserving the first durable cause.
#[must_use]
pub fn durable_run_terminal_v1(
    state: RunState,
    failure_code: Option<ErrorCode>,
    budget_exhausted: bool,
    deadline_exceeded: bool,
    blocking_error_code: Option<&'static str>,
) -> Option<DurableRunTerminalV1> {
    match state {
        RunState::Cancelled | RunState::Errored if deadline_exceeded => {
            Some(DurableRunTerminalV1 {
                terminal_kind: "timeout",
                error_code: Some("timeout"),
            })
        }
        RunState::Done => Some(DurableRunTerminalV1 {
            terminal_kind: "success",
            error_code: None,
        }),
        RunState::Cancelled | RunState::Errored if blocking_error_code.is_some() => {
            Some(DurableRunTerminalV1 {
                terminal_kind: "failure",
                error_code: blocking_error_code,
            })
        }
        RunState::Cancelled => Some(DurableRunTerminalV1 {
            terminal_kind: "cancellation",
            error_code: None,
        }),
        RunState::Errored
            if budget_exhausted || matches!(failure_code, Some(ErrorCode::BudgetExhausted)) =>
        {
            Some(DurableRunTerminalV1 {
                terminal_kind: "budget",
                error_code: Some("budget_exhausted"),
            })
        }
        RunState::Errored
            if matches!(
                failure_code,
                Some(ErrorCode::ProviderError | ErrorCode::ProviderTimeout)
            ) =>
        {
            Some(DurableRunTerminalV1 {
                terminal_kind: "provider_error",
                error_code: match failure_code {
                    Some(code) => Some(code.as_str()),
                    None => None,
                },
            })
        }
        RunState::Errored => Some(DurableRunTerminalV1 {
            terminal_kind: "failure",
            error_code: match failure_code {
                Some(code) => Some(code.as_str()),
                None => Some("internal"),
            },
        }),
        _ => None,
    }
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

#[cfg(test)]
#[path = "headless_tests.rs"]
mod tests;
