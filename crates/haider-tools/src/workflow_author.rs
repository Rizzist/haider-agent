//! Conditionally granted child-only graph replacement proposal.

use crate::{ToolError, ToolResult};
use haider_protocol::graph::{GraphTemplateSpec, validate_graph_template};
use haider_protocol::tool::{DispatchMode, ToolManifest};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_WORKFLOW_AUTHOR_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowAuthor {
    pub template: GraphTemplateSpec,
}

impl WorkflowAuthor {
    pub fn from_tool_args(args: Value) -> ToolResult<Self> {
        let encoded = serde_json::to_vec(&args).map_err(|error| ToolError::Runtime {
            message: format!("cannot encode workflow_author arguments: {error}"),
        })?;
        if encoded.len() > MAX_WORKFLOW_AUTHOR_BYTES {
            return Err(ToolError::invalid_argument(format!(
                "workflow_author arguments exceed {MAX_WORKFLOW_AUTHOR_BYTES} bytes"
            )));
        }
        let request: Self = serde_json::from_value(args).map_err(|error| {
            ToolError::invalid_argument(format!("invalid workflow_author arguments: {error}"))
        })?;
        validate_graph_template(&request.template).map_err(|error| {
            ToolError::invalid_argument(format!("invalid authored workflow: {error}"))
        })?;
        Ok(request)
    }
}

#[must_use]
pub fn workflow_author_manifest() -> ToolManifest {
    ToolManifest {
        name: "workflow_author".into(),
        description: "Replace this workflow-enabled child session's initial graph with one bounded, validated DAG. This capability is present only when the daemon granted a deeper-workflow authoring request."
            .into(),
        effects: vec![],
        dispatch: DispatchMode::Await,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "template": {
                    "type": "object",
                    "description": "GraphTemplateSpec: name, version, start_node, and bounded nodes",
                    "required": ["name", "version", "start_node", "nodes"]
                }
            },
            "required": ["template"],
            "additionalProperties": false
        }),
    }
}
