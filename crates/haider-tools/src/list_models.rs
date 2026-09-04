use crate::{ToolError, ToolResult};
use haider_protocol::tool::{DispatchMode, ToolManifest};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const LIST_MODELS_FILTER_MAX_BYTES: usize = 128;
pub const LIST_MODELS_ROW_CAP: usize = 100;

/// Validated arguments for the daemon-cached model catalog tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListModels {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
}

impl ListModels {
    pub fn from_tool_args(args: Value) -> ToolResult<Self> {
        let request: Self = serde_json::from_value(args).map_err(|error| {
            ToolError::invalid_argument(format!("invalid list_models arguments: {error}"))
        })?;
        let filter = request
            .filter
            .map(|filter| filter.trim().to_owned())
            .filter(|filter| !filter.is_empty());
        if filter
            .as_ref()
            .is_some_and(|filter| filter.len() > LIST_MODELS_FILTER_MAX_BYTES)
        {
            return Err(ToolError::invalid_argument(format!(
                "list_models filter must contain 1..={LIST_MODELS_FILTER_MAX_BYTES} bytes when given"
            )));
        }
        Ok(Self { filter })
    }
}

/// Frozen manifest shape advertised to providers and policy projection.
pub fn list_models_manifest() -> ToolManifest {
    ToolManifest {
        name: "list_models".into(),
        description: "List the daemon's already-discovered model catalog. This is a cached local read and never refreshes provider inventory. Use filter when the bounded result is truncated."
            .into(),
        effects: vec![],
        dispatch: DispatchMode::Await,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "filter": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": LIST_MODELS_FILTER_MAX_BYTES,
                    "description": "Optional case-insensitive model/provider/alias substring"
                }
            },
            "additionalProperties": false
        }),
    }
}
