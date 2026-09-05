//! Retained evidence for the harness's logical provider-request ceiling.

use crate::request_budget::RequestBudgetContinuationV1;
use serde::{Deserialize, Serialize};

/// Stable CLI exit status for `request_budget_exceeded`, distinct from
/// software failure (70) and permission/shared-budget blocking (77).
pub const INTERNAL_CEILING_EXIT_CODE: u8 = 78;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunEndReasonV1 {
    HarnessInternalCeiling,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceStateV1 {
    Mutated,
    Untouched,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnCeilingV1 {
    pub soft: usize,
    pub hard: usize,
    pub used: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartialProgressV1 {
    /// Sorted workspace-relative paths added or changed between receipts.
    /// This is a net tree comparison, not attribution to a particular tool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files_written: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files_deleted: Option<Vec<String>>,
    /// Tool calls with a durable ToolResult in this run, including reused IDs.
    pub tool_calls: usize,
    /// Last allocated physical provider-request ordinal, including retries.
    pub last_request_ordinal: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceReceiptPhaseV1 {
    Before,
    After,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceReceiptErrorV1 {
    pub phase: WorkspaceReceiptPhaseV1,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InternalCeilingTerminalV1 {
    pub end_reason: RunEndReasonV1,
    pub internal_cap_detected: bool,
    pub exit_code: u8,
    pub ceilings: TurnCeilingV1,
    pub continuation: RequestBudgetContinuationV1,
    /// Absent only with a typed receipt error; never invent an untouched tree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_state: Option<WorkspaceStateV1>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_before: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_after: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_receipt_error: Option<WorkspaceReceiptErrorV1>,
    pub partial_progress: PartialProgressV1,
}

impl InternalCeilingTerminalV1 {
    /// Only a typed, self-consistent retained terminal is cap evidence.
    /// Human text, a soft warning, and arbitrary JSON cannot classify a cap.
    #[must_use]
    pub fn from_payload(payload: &serde_json::Value) -> Option<Self> {
        if payload.get("type")?.as_str()? != "run_state"
            || payload.get("state")?.as_str()? != "errored"
        {
            return None;
        }
        let terminal: Self = serde_json::from_value(payload.get("terminal")?.clone()).ok()?;
        (terminal.internal_cap_detected
            && terminal.exit_code == INTERNAL_CEILING_EXIT_CODE
            && terminal.ceilings.soft > 0
            && terminal.ceilings.soft <= terminal.ceilings.hard
            && terminal.ceilings.used >= terminal.ceilings.hard
            && terminal.valid_workspace_evidence())
        .then_some(terminal)
    }

    fn valid_workspace_evidence(&self) -> bool {
        match (&self.workspace_state, &self.workspace_receipt_error) {
            (Some(state), None) => {
                self.workspace_before
                    .as_ref()
                    .is_some_and(|value| !value.is_empty())
                    && self
                        .workspace_after
                        .as_ref()
                        .is_some_and(|value| !value.is_empty())
                    && ((self.workspace_before == self.workspace_after)
                        == (*state == WorkspaceStateV1::Untouched))
                    && self.partial_progress.files_written.is_some()
                    && self.partial_progress.files_deleted.is_some()
            }
            (None, Some(_)) => {
                self.workspace_after.is_none()
                    && self.partial_progress.files_written.is_none()
                    && self.partial_progress.files_deleted.is_none()
            }
            _ => false,
        }
    }
}
