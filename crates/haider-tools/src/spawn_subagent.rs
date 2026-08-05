use crate::{EffectOperation, ToolError, ToolResult};
use haider_protocol::effect::EffectClass;
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
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
        Ok(Self {
            task: task.to_owned(),
            prompt: prompt.to_owned(),
            model,
            provider,
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
                    "description": "Optional model for the child; omitted, the child inherits this session's current model"
                },
                "provider": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_SELECTOR_BYTES,
                    "description": "Optional disambiguator when `model` is served by several providers; requires `model`"
                }
            },
            "required": ["task", "prompt"],
            "additionalProperties": false
        }),
    }
}
