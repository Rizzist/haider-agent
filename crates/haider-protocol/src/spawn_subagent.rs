//! Shared spawn argument validation for CLI and tool callers.

use crate::graph::{ChildWorkflowSelector, ChildWorkflowTrigger};
use crate::ids::{AgentId, SessionId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAX_TASK_BYTES: usize = 80;
pub const MAX_PROMPT_BYTES: usize = 32 * 1024;
pub const MAX_SELECTOR_BYTES: usize = 128;
/// Upper bound on a blocking spawn's `wait_ms` observation window: one day.
pub const MAX_WAIT_MS: u64 = 86_400_000;

/// CLI command that collects a delegated child's report by its handle.
pub fn agent_wait_command(parent_session_id: &SessionId, agent: &AgentId) -> String {
    format!("haider agent wait {parent_session_id} {agent} --json")
}

/// CLI command that lists a parent session's delegated-child handles.
pub fn agent_list_command(parent_session_id: &SessionId) -> String {
    format!("haider agent list {parent_session_id} --json")
}

/// CLI command that cancels a delegated child by its handle.
pub fn agent_cancel_command(parent_session_id: &SessionId, agent: &AgentId) -> String {
    format!("haider agent cancel {parent_session_id} {agent} --json")
}

/// Validated arguments for the depth-capped local-subagent tool.
///
/// `model`/`provider` are the ADDITIVE model selector (F1). Sessions are
/// provider-agnostic: absent, the child inherits the parent's CURRENT model
/// pair; a bare `model` resolves to a pair through the daemon's one selection
/// authority; `provider` only disambiguates a model served by several
/// providers. Both absent keeps legacy argument bytes byte-for-byte
/// (`skip_serializing_if`), so historical receipts and effect summaries are
/// unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnSubagent {
    pub task: String,
    pub prompt: String,
    /// Return the durable child handle immediately instead of waiting for its report.
    #[serde(default, skip_serializing_if = "is_false")]
    pub background: bool,
    /// Optional bound on this parent's observation, never on the child's life.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_ms: Option<u64>,
    /// Per-turn request tranche and hard ceiling pinned for this child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_budget: Option<crate::request_budget::RequestBudgetV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<ChildWorkflowSelector>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_trigger: Option<ChildWorkflowTrigger>,
    /// Declared parent evidence slot which receives the single collapsed
    /// terminal child contract. Ignored on the bare-attempt path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_slot: Option<String>,
    /// A proposal to let a workflow child replace its initial pinned graph.
    /// The daemon grants this only for a deeper-workflow trigger.
    #[serde(default, skip_serializing_if = "is_false")]
    pub workflow_author: bool,
    /// C2 — a registered Loom agent type: the daemon injects the type's Job
    /// as the child's role and (B3) scopes the child to the type's grants.
    /// Unknown types reject as a completed tool result, never a turn failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
}

// Serde derives both scalar and short-sequence expectations from the Rust
// name. A rename does not change them, while expecting overrides the sequence
// element-count suffix too. Keep the legacy name as the derive authority and
// expose the shared arguments under their existing public alias.
pub use SpawnSubagent as SpawnSubagentArguments;

impl SpawnSubagent {
    pub fn from_tool_args(args: Value) -> Result<Self, String> {
        let request: Self = serde_json::from_value(args)
            .map_err(|error| format!("invalid spawn_subagent arguments: {error}"))?;
        let task = request.task.trim();
        let prompt = request.prompt.trim();
        if task.is_empty() || task.len() > MAX_TASK_BYTES {
            return Err(format!(
                "spawn_subagent task must contain 1..={MAX_TASK_BYTES} bytes"
            ));
        }
        if prompt.is_empty() || prompt.len() > MAX_PROMPT_BYTES {
            return Err(format!(
                "spawn_subagent prompt must contain 1..={MAX_PROMPT_BYTES} bytes"
            ));
        }
        let selector = |value: Option<String>, name: &str| -> Result<Option<String>, String> {
            let Some(value) = value else { return Ok(None) };
            let value = value.trim();
            if value.is_empty() || value.len() > MAX_SELECTOR_BYTES {
                return Err(format!(
                    "spawn_subagent {name} must contain 1..={MAX_SELECTOR_BYTES} bytes when given"
                ));
            }
            Ok(Some(value.to_owned()))
        };
        let model = selector(request.model, "model")?;
        let provider = selector(request.provider, "provider")?;
        if provider.is_some() && model.is_none() {
            return Err(
                "spawn_subagent `provider` only disambiguates a `model` — name the model"
                    .to_owned(),
            );
        }
        let parent_slot = selector(request.parent_slot, "parent_slot")?;
        let agent_type = selector(request.agent_type, "agent_type")?;
        if let Some(budget) = request.request_budget {
            budget.validate()?;
        }
        if request
            .wait_ms
            .is_some_and(|wait_ms| wait_ms == 0 || wait_ms > MAX_WAIT_MS)
        {
            return Err(format!(
                "spawn_subagent wait_ms must be in 1..={MAX_WAIT_MS}"
            ));
        }
        if request.background && request.wait_ms.is_some() {
            return Err("spawn_subagent wait_ms is only valid for a blocking spawn".into());
        }
        Ok(Self {
            task: task.to_owned(),
            prompt: prompt.to_owned(),
            background: request.background,
            wait_ms: request.wait_ms,
            request_budget: request.request_budget,
            model,
            provider,
            workflow: request.workflow,
            workflow_trigger: request.workflow_trigger,
            parent_slot,
            workflow_author: request.workflow_author,
            agent_type,
        })
    }
}

const fn is_false(value: &bool) -> bool {
    !*value
}
