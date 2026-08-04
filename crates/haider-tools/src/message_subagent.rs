use crate::{ToolError, ToolResult};
use haider_protocol::ids::AgentId;
use haider_protocol::tool::{DispatchMode, ToolManifest};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_MESSAGE_BYTES: usize = 32 * 1024;

/// Validated parent-to-direct-child message arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageSubagent {
    pub agent: AgentId,
    pub message: String,
}

impl MessageSubagent {
    pub fn from_tool_args(args: Value) -> ToolResult<Self> {
        let request: Self = serde_json::from_value(args).map_err(|error| {
            ToolError::invalid_argument(format!("invalid message_subagent arguments: {error}"))
        })?;
        let message = request.message.trim();
        if request.agent.as_str().trim().is_empty() {
            return Err(ToolError::invalid_argument(
                "message_subagent agent must not be empty",
            ));
        }
        if message.is_empty() || message.len() > MAX_MESSAGE_BYTES {
            return Err(ToolError::invalid_argument(format!(
                "message_subagent message must contain 1..={MAX_MESSAGE_BYTES} bytes"
            )));
        }
        Ok(Self {
            agent: request.agent,
            message: message.to_owned(),
        })
    }
}

/// Frozen provider/policy manifest for direct-child messaging.
pub fn message_subagent_manifest() -> ToolManifest {
    ToolManifest {
        name: "message_subagent".into(),
        description: "Message one of your direct child agents. Running children are steered in their current round; idle children start immediately. Shared specs may be placed in the EPHEMERAL <workspace>/.haider/handoff/<session-short>/ directory.".into(),
        effects: Vec::new(),
        dispatch: DispatchMode::Await,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "agent": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Opaque child agent id returned by spawn_subagent"
                },
                "message": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_MESSAGE_BYTES,
                    "description": "Instruction to inject or start as the child's next turn"
                }
            },
            "required": ["agent", "message"],
            "additionalProperties": false
        }),
    }
}
