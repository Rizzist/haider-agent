//! Logical provider-request tranches and durable continuation coordinates.

use crate::ids::{AgentId, BranchId, RunId, SessionId};
use crate::item::TurnItem;
use serde::{Deserialize, Serialize};

pub const PROVIDER_REQUEST_BUDGET_EXTENSION_KIND: &str = "provider_request_budget_v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestBudgetV1 {
    pub tranche: usize,
    pub hard_cap: usize,
}

impl Default for RequestBudgetV1 {
    fn default() -> Self {
        Self {
            tranche: 32,
            // Two 32-request tranches leave 11 requests beyond the observed
            // 53-round solved benchmark while retaining a finite loop guard.
            hard_cap: 64,
        }
    }
}

impl RequestBudgetV1 {
    pub fn validate(self) -> Result<(), String> {
        if self.tranche == 0 || self.hard_cap == 0 || self.tranche > self.hard_cap {
            Err("request budget requires 0 < tranche <= hard_cap".into())
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestBudgetPhaseV1 {
    Progress,
    SoftBound,
    HardBound,
}

/// A continuation starts a fresh turn in this session/branch. The terminal
/// source run remains immutable and its committed tool history is retained.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestBudgetContinuationV1 {
    pub session_id: SessionId,
    pub run_id: RunId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_id: Option<BranchId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<AgentId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestBudgetStatusV1 {
    pub used: usize,
    pub budget: RequestBudgetV1,
    pub phase: RequestBudgetPhaseV1,
    pub continuation: RequestBudgetContinuationV1,
}

impl RequestBudgetStatusV1 {
    #[must_use]
    pub fn summary(&self) -> String {
        let phase = match self.phase {
            RequestBudgetPhaseV1::Progress => "in progress",
            RequestBudgetPhaseV1::SoftBound => "soft tranche reached; finish or checkpoint",
            RequestBudgetPhaseV1::HardBound => "hard cap reached; continuation available",
        };
        let mut summary = format!(
            "requests {} / tranche {} / hard cap {} — {phase}",
            self.used, self.budget.tranche, self.budget.hard_cap
        );
        if self.phase != RequestBudgetPhaseV1::Progress {
            summary.push_str(&format!("; resume {}", self.continuation.run_id));
        }
        summary
    }

    /// Typed JSON wrapped in an explicit instruction for provider messages.
    #[must_use]
    pub fn model_note(&self) -> String {
        let instruction = match self.phase {
            RequestBudgetPhaseV1::Progress => {
                "This records the requests used by the referenced turn."
            }
            RequestBudgetPhaseV1::SoftBound => {
                "You have used your soft request tranche. Finish the task or record a checkpoint with completed work and concrete next steps. Continue within the remaining hard-cap requests."
            }
            RequestBudgetPhaseV1::HardBound => {
                "The referenced turn stopped at its hard request cap. Its checkpoint, committed messages, and tool history are preserved. Resume in a new turn with a fresh request allowance and continue from the retained work."
            }
        };
        format!(
            "[provider_request_budget_v1]\n{}\n{instruction} This note applies only to continuation.run_id; later turns have a fresh request budget.\n[/provider_request_budget_v1]",
            serde_json::json!({
                "type": PROVIDER_REQUEST_BUDGET_EXTENSION_KIND,
                "used": self.used,
                "budget": self.budget,
                "phase": self.phase,
                "continuation": self.continuation,
            })
        )
    }

    pub fn to_extension_item(&self) -> Result<TurnItem, serde_json::Error> {
        let mut data = serde_json::to_value(self)?;
        data["label"] = serde_json::Value::String(self.summary());
        Ok(TurnItem::Extension {
            kind: PROVIDER_REQUEST_BUDGET_EXTENSION_KIND.into(),
            data,
        })
    }

    #[must_use]
    pub fn from_extension_item(item: &TurnItem) -> Option<Self> {
        let TurnItem::Extension { kind, data } = item else {
            return None;
        };
        if kind != PROVIDER_REQUEST_BUDGET_EXTENSION_KIND {
            return None;
        }
        let status: Self = serde_json::from_value(data.clone()).ok()?;
        status.budget.validate().ok()?;
        Some(status)
    }
}
