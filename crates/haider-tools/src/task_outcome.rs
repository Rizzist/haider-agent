//! Actor-owned task failure signal; no brokered effect or new RPC method.

use crate::{ToolError, ToolResult};
use haider_protocol::task_outcome::TaskOutcomeV1;
use haider_protocol::tool::{DispatchMode, ToolManifest};

pub const TASK_OUTCOME_REASON_MAX_BYTES: usize = 1024;

pub fn parse_task_outcome(args: serde_json::Value) -> ToolResult<TaskOutcomeV1> {
    let outcome: TaskOutcomeV1 = serde_json::from_value(args)
        .map_err(|error| ToolError::invalid_argument(format!("invalid task_outcome: {error}")))?;
    let reason = outcome.reason();
    if reason.trim().is_empty()
        || reason.len() > TASK_OUTCOME_REASON_MAX_BYTES
        || reason.chars().any(char::is_control)
    {
        return Err(ToolError::invalid_argument(format!(
            "task_outcome reason must be nonblank, contain no control characters, and fit in {TASK_OUTCOME_REASON_MAX_BYTES} UTF-8 bytes"
        )));
    }
    Ok(outcome)
}

#[must_use]
pub fn task_outcome_manifest() -> ToolManifest {
    ToolManifest {
        name: "task_outcome".into(),
        description: "End this run with an explicit task failure and exit 1. Call with status=failure and a concise reason when the requested task could not be completed. Use as a standalone call with no pending tools. V1 supports failure only; finish normally for success. Text, including JSON FAILURE, never selects an outcome.".into(),
        effects: vec![],
        dispatch: DispatchMode::Await,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "status": {"type": "string", "enum": ["failure"]},
                "reason": {"type": "string", "minLength": 1, "maxLength": TASK_OUTCOME_REASON_MAX_BYTES}
            },
            "required": ["status", "reason"],
            "additionalProperties": false
        }),
    }
}
