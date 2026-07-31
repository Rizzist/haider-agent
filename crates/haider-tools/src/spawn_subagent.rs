use crate::{EffectOperation, ToolError, ToolResult};
use haider_protocol::effect::EffectClass;
use haider_protocol::tool::{DispatchMode, ToolManifest};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_TASK_BYTES: usize = 80;
const MAX_PROMPT_BYTES: usize = 32 * 1024;

/// Validated arguments for the depth-capped local-subagent tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnSubagent {
    pub task: String,
    pub prompt: String,
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
        Ok(Self {
            task: task.to_owned(),
            prompt: prompt.to_owned(),
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
        description: "Delegate one bounded task to a depth-capped local child agent".into(),
        effects: vec![EffectClass::AgentSpawn],
        dispatch: DispatchMode::Deferred,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
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
                }
            },
            "required": ["task", "prompt"],
            "additionalProperties": false
        }),
    }
}
