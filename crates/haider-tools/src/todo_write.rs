//! The model-facing plan surface (G1).
//!
//! `todo_write` deliberately does not enter the effect permission broker:
//! stating a plan is not a side effect (the `request_input` precedent). The
//! owning actor journals the `TurnItem::Plan` lifecycle facts and returns a
//! compact count echo as the tool result. Semantics are WHOLE-LIST REPLACE
//! per call (Claude Code `TodoWrite`): items carry stable ids, so the model
//! updates or completes a specific todo by re-sending the list with that
//! item's `state` changed.

use crate::{ToolError, ToolResult};
use haider_protocol::history::{TodoItem, TodoState};
use haider_protocol::tool::{DispatchMode, ToolManifest};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAX_TODO_ITEMS: usize = 50;
pub const MAX_TODO_TEXT_CHARS: usize = 500;

/// Validated whole-list `todo_write` arguments. `items` reuses the protocol
/// [`TodoItem`] verbatim — no parallel shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TodoWrite {
    pub items: Vec<TodoItem>,
}

impl TodoWrite {
    /// Parses and validates arguments, tolerating the Claude Code `TodoWrite`
    /// vocabulary: top-level `todos` for `items`, per-item `content` for
    /// `text`, `status` for `state` with `pending`/`in_progress` mapped to
    /// `listed`/`processing`, ids assigned by position when EVERY item omits
    /// one, and unknown extra fields (for example `activeForm`) ignored.
    pub fn from_tool_args(args: Value) -> ToolResult<Self> {
        let repaired = repair_args(args)?;
        let request: Self = serde_json::from_value(repaired).map_err(|error| {
            ToolError::invalid_argument(format!("invalid todo_write arguments: {error}"))
        })?;
        request.validate()?;
        Ok(request)
    }

    /// Every item is `completed`. An EMPTY list is not a completed plan — it
    /// is a clear — so this is false for it.
    #[must_use]
    pub fn all_completed(&self) -> bool {
        !self.items.is_empty()
            && self
                .items
                .iter()
                .all(|item| item.state == TodoState::Completed)
    }

    /// Compact model-facing echo: `{"ok": true, "counts": {...}}`. Replayed
    /// verbatim into later prompts, it carries the acknowledged state.
    #[must_use]
    pub fn result_echo(&self) -> Value {
        let count = |state: TodoState| self.items.iter().filter(|item| item.state == state).count();
        serde_json::json!({
            "ok": true,
            "counts": {
                "listed": count(TodoState::Listed),
                "processing": count(TodoState::Processing),
                "completed": count(TodoState::Completed),
            }
        })
    }

    fn validate(&self) -> ToolResult<()> {
        if self.items.len() > MAX_TODO_ITEMS {
            return Err(ToolError::invalid_argument(format!(
                "todo_write accepts at most {MAX_TODO_ITEMS} items, got {}",
                self.items.len()
            )));
        }
        let mut ids = std::collections::HashSet::new();
        for item in &self.items {
            if !ids.insert(item.id) {
                return Err(ToolError::invalid_argument(format!(
                    "duplicate todo id {}",
                    item.id
                )));
            }
            if item.text.trim().is_empty() {
                return Err(ToolError::invalid_argument(format!(
                    "todo {} text must not be empty",
                    item.id
                )));
            }
            if item.text.chars().count() > MAX_TODO_TEXT_CHARS {
                return Err(ToolError::invalid_argument(format!(
                    "todo {} text must contain at most {MAX_TODO_TEXT_CHARS} characters",
                    item.id
                )));
            }
        }
        for item in &self.items {
            if let Some(dep) = item.dep
                && !ids.contains(&dep)
            {
                return Err(ToolError::invalid_argument(format!(
                    "todo {} depends on unknown id {dep}",
                    item.id
                )));
            }
        }
        // `dep` is a single edge per item, so a cycle is a chain that
        // revisits a node. Following each chain at most items.len() hops
        // detects every cycle (including a self-dep) in this small list.
        for start in &self.items {
            let mut current = start.dep;
            let mut hops = 0;
            while let Some(dep) = current {
                if dep == start.id {
                    return Err(ToolError::invalid_argument(format!(
                        "todo dependency cycle through id {}",
                        start.id
                    )));
                }
                hops += 1;
                if hops > self.items.len() {
                    break;
                }
                current = self
                    .items
                    .iter()
                    .find(|item| item.id == dep)
                    .and_then(|item| item.dep);
            }
        }
        Ok(())
    }
}

/// Claude-Code-vocabulary key repair. Canonical keys always win; repair only
/// fills gaps. Non-object shapes fall through untouched so serde reports the
/// real type error.
fn repair_args(mut args: Value) -> ToolResult<Value> {
    let Some(top) = args.as_object_mut() else {
        return Ok(args);
    };
    if !top.contains_key("items")
        && let Some(todos) = top.remove("todos")
    {
        top.insert("items".into(), todos);
    }
    let Some(items) = top.get_mut("items").and_then(Value::as_array_mut) else {
        return Ok(args);
    };
    let with_id = items.iter().filter(|item| item.get("id").is_some()).count();
    if with_id != 0 && with_id != items.len() {
        return Err(ToolError::invalid_argument(
            "todo_write items must either all carry an `id` or all omit it",
        ));
    }
    let assign_positional_ids = with_id == 0;
    for (index, item) in items.iter_mut().enumerate() {
        let Some(fields) = item.as_object_mut() else {
            continue;
        };
        if assign_positional_ids {
            fields.insert("id".into(), Value::from(index as u32));
        }
        if !fields.contains_key("text")
            && let Some(content) = fields.remove("content")
        {
            fields.insert("text".into(), content);
        }
        if !fields.contains_key("state")
            && let Some(status) = fields.remove("status")
        {
            fields.insert("state".into(), status);
        }
        if let Some(state) = fields.get_mut("state")
            && let Some(value) = state.as_str()
        {
            let repaired = match value {
                "pending" => Some("listed"),
                "in_progress" => Some("processing"),
                _ => None,
            };
            if let Some(repaired) = repaired {
                *state = Value::from(repaired);
            }
        }
    }
    Ok(args)
}

/// Frozen manifest shape advertised to providers and policy projection. The
/// description IS the model-facing usage spec.
pub fn todo_write_manifest() -> ToolManifest {
    ToolManifest {
        name: "todo_write".into(),
        description: "Maintain your visible task plan. Use it for multi-step work: list the steps \
            before starting, keep exactly one item `processing` while you work on it, and mark an \
            item `completed` immediately when it is done. Every call REPLACES the whole list, so \
            re-send all items each time, keeping each item's `id` stable; change one item's \
            `state` to update or complete that specific todo. `dep` marks an item blocked until \
            the referenced id completes. An empty list clears the plan. Not needed for trivial \
            single-step tasks."
            .into(),
        effects: vec![],
        dispatch: DispatchMode::Await,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "maxItems": MAX_TODO_ITEMS,
                    "description": "The COMPLETE plan; this list replaces the previous one",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {
                                "type": "integer",
                                "description": "Stable id for this todo, kept across calls"
                            },
                            "text": {
                                "type": "string",
                                "minLength": 1,
                                "maxLength": MAX_TODO_TEXT_CHARS,
                                "description": "Short imperative description"
                            },
                            "state": {
                                "type": "string",
                                "enum": ["listed", "processing", "completed"],
                                "description": "listed = not started, processing = in progress (one at a time), completed = done"
                            },
                            "dep": {
                                "type": "integer",
                                "description": "Optional id of the todo this one is blocked on"
                            }
                        },
                        "required": ["id", "text", "state"]
                    }
                }
            },
            "required": ["items"]
        }),
    }
}
