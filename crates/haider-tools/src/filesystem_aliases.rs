//! Flat model-facing schemas decode into the existing transactional operations.

use crate::{FsEdit, FsWrite, ToolError, ToolResult};
use haider_protocol::effect::EffectClass;
use haider_protocol::tool::{DispatchMode, ToolManifest};
use serde::Deserialize;
use serde_json::{Value, json};

pub fn edit_manifest() -> ToolManifest {
    ToolManifest {
        name: "edit".into(),
        description: "Replace exact text in one UTF-8 file after a fresh read; the anchor must be unique unless replace_all is true".into(),
        effects: vec![EffectClass::FsWrite],
        dispatch: DispatchMode::Await,
        input_schema: json!({
            "type": "object",
            "properties": {
                "file_path": {"type": "string", "minLength": 1, "description": "Path inside the workspace; read the current file before editing"},
                "old_string": {"type": "string", "minLength": 1, "description": "Exact existing text, including whitespace; must match uniquely unless replace_all"},
                "new_string": {"type": "string", "description": "Replacement text; an empty string deletes the matched text"},
                "replace_all": {"type": "boolean", "description": "Replace every exact match; at least one match is required; defaults to false"}
            },
            "required": ["file_path", "old_string", "new_string"],
            "additionalProperties": false
        }),
    }
}

pub fn write_manifest() -> ToolManifest {
    ToolManifest {
        name: "write".into(),
        description:
            "Create or replace one UTF-8 file; replacing an existing file requires a fresh read"
                .into(),
        effects: vec![EffectClass::FsWrite],
        dispatch: DispatchMode::Await,
        input_schema: json!({
            "type": "object",
            "properties": {
                "file_path": {"type": "string", "minLength": 1, "description": "Path inside the workspace; read an existing file before replacing it"},
                "content": {"type": "string", "description": "Complete UTF-8 file contents; an empty string creates or truncates to an empty file"}
            },
            "required": ["file_path", "content"],
            "additionalProperties": false
        }),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditArgs {
    file_path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    file_path: String,
    content: String,
}

impl FsEdit {
    /// Decode the flat `edit` alias without changing any broker guarantees.
    pub fn from_edit_args(args: &Value) -> ToolResult<Self> {
        let args: EditArgs = serde_json::from_value(args.clone()).map_err(|error| {
            ToolError::invalid_argument(format!("invalid edit arguments: {error}"))
        })?;
        require_nonempty(&args.file_path, "file_path")?;
        require_nonempty(&args.old_string, "old_string")?;
        Ok(Self::new(args.file_path, args.old_string, args.new_string)
            .replace_all(args.replace_all))
    }
}

impl FsWrite {
    /// Decode the flat `write` alias without changing any broker guarantees.
    pub fn from_write_args(args: &Value) -> ToolResult<Self> {
        let args: WriteArgs = serde_json::from_value(args.clone()).map_err(|error| {
            ToolError::invalid_argument(format!("invalid write arguments: {error}"))
        })?;
        require_nonempty(&args.file_path, "file_path")?;
        Ok(Self::new(args.file_path, args.content))
    }
}

fn require_nonempty(value: &str, field: &str) -> ToolResult<()> {
    if value.is_empty() {
        return Err(ToolError::invalid_argument(format!(
            "tool argument `{field}` must be a non-empty string"
        )));
    }
    Ok(())
}
