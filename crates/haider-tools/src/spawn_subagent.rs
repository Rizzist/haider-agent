use crate::{EffectOperation, ToolError, ToolResult};
use haider_protocol::effect::EffectClass;
use haider_protocol::graph::{ChildWorkflowSelector, ChildWorkflowTrigger};
use haider_protocol::tool::{DispatchMode, ToolManifest};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_TASK_BYTES: usize = 80;
const MAX_PROMPT_BYTES: usize = 32 * 1024;
const MAX_SELECTOR_BYTES: usize = 128;

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
    /// Per-turn request tranche and hard ceiling pinned for this child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_budget: Option<haider_protocol::request_budget::RequestBudgetV1>,
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

impl SpawnSubagent {
    pub fn from_tool_args(args: Value) -> ToolResult<Self> {
        let request: Self = serde_json::from_value(args).map_err(|error| {
            ToolError::invalid_argument(format!("invalid spawn_subagent arguments: {error}"))
        })?;
        let task = request.task.trim();
        let prompt = request.prompt.trim();
        if task.is_empty() || task.len() > MAX_TASK_BYTES {
            return Err(ToolError::invalid_argument(format!(
                "spawn_subagent task must contain 1..={MAX_TASK_BYTES} bytes"
            )));
        }
        if prompt.is_empty() || prompt.len() > MAX_PROMPT_BYTES {
            return Err(ToolError::invalid_argument(format!(
                "spawn_subagent prompt must contain 1..={MAX_PROMPT_BYTES} bytes"
            )));
        }
        let selector = |value: Option<String>, name: &str| -> ToolResult<Option<String>> {
            let Some(value) = value else { return Ok(None) };
            let value = value.trim();
            if value.is_empty() || value.len() > MAX_SELECTOR_BYTES {
                return Err(ToolError::invalid_argument(format!(
                    "spawn_subagent {name} must contain 1..={MAX_SELECTOR_BYTES} bytes when given"
                )));
            }
            Ok(Some(value.to_owned()))
        };
        let model = selector(request.model, "model")?;
        let provider = selector(request.provider, "provider")?;
        if provider.is_some() && model.is_none() {
            return Err(ToolError::invalid_argument(
                "spawn_subagent `provider` only disambiguates a `model` — name the model"
                    .to_owned(),
            ));
        }
        let parent_slot = selector(request.parent_slot, "parent_slot")?;
        let agent_type = selector(request.agent_type, "agent_type")?;
        if let Some(budget) = request.request_budget {
            budget.validate().map_err(ToolError::invalid_argument)?;
        }
        Ok(Self {
            task: task.to_owned(),
            prompt: prompt.to_owned(),
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

impl EffectOperation for SpawnSubagent {
    fn effect_class(&self) -> EffectClass {
        EffectClass::AgentSpawn
    }

    fn summary(&self) -> String {
        format!("spawn local subagent for {}", self.task)
    }

    fn arguments(&self) -> ToolResult<Value> {
        serde_json::to_value(self).map_err(|error| ToolError::Runtime {
            message: format!("cannot encode spawn_subagent arguments: {error}"),
        })
    }
}

/// Frozen manifest shape advertised to providers and policy projection.
pub fn spawn_subagent_manifest() -> ToolManifest {
    ToolManifest {
        name: "spawn_subagent".into(),
        description: "Delegate one bounded task to a depth-capped local child agent. Shared specs may be placed in the EPHEMERAL <workspace>/.haider/handoff/<session-short>/ directory.".into(),
        effects: vec![EffectClass::AgentSpawn],
        dispatch: DispatchMode::Deferred,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "request_budget": {
                    "type": "object",
                    "description": "Optional child per-turn provider request budget; defaults to tranche 32 and hard cap 64. Transport retries are excluded. Must satisfy tranche <= hard_cap.",
                    "properties": {
                        "tranche": {"type": "integer", "minimum": 1},
                        "hard_cap": {"type": "integer", "minimum": 1}
                    },
                    "required": ["tranche", "hard_cap"],
                    "additionalProperties": false
                },
                "task": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_TASK_BYTES,
                    "description": "Short display label for the child"
                },
                "prompt": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_PROMPT_BYTES,
                    "description": "Complete task prompt for the child"
                },
                "model": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_SELECTOR_BYTES,
                    "description": "Optional model for the child; omitted, the child inherits this session's current model. A bare selector matches case-insensitively and ignores '-', '_', '.', and whitespace; literal exact matches win. Call list_models to inspect valid model/provider pairs."
                },
                "provider": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_SELECTOR_BYTES,
                    "description": "Optional disambiguator when `model` is served by several providers; requires `model`"
                },
                "workflow": {
                    "type": "string",
                    "pattern": "^(plain|implement_verify|deeper|workflow_ref\\([A-Za-z0-9_-]{1,64}\\))$",
                    "description": "Optional child workflow proposal; omitted defaults to one bare attempt"
                },
                "workflow_trigger": {
                    "type": "string",
                    "enum": ["mutation_with_independent_verification", "dependent_phases", "fan_out", "distinct_review", "crash_recovery"],
                    "description": "Bounded reason the workflow earns its graph"
                },
                "parent_slot": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_SELECTOR_BYTES,
                    "description": "Declared parent graph evidence slot for the single collapsed child contract"
                },
                "agent_type": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 64,
                    "description": "Registered Loom agent type for a TYPED child (capability-scoped specialist)"
                },
                "workflow_author": {
                    "type": "boolean",
                    "description": "Request graph-authoring capability; granted only for a deeper trigger"
                }
            },
            "required": ["task", "prompt"],
            "additionalProperties": false
        }),
    }
}

const fn is_false(value: &bool) -> bool {
    !*value
}
