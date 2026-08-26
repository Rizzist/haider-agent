//! Typed model-facing vocabulary for session monitors.
//!
//! This crate owns only strict parsing and the provider manifest. Runtime
//! registration, durable projection, source subscriptions, and wake delivery
//! belong to `haider-daemon::monitor`.

use crate::{ToolError, ToolResult};
use haider_protocol::tool::{DispatchMode, ToolManifest};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAX_MONITOR_ID_CHARS: usize = 96;
pub const MAX_MONITOR_FILTER_CHARS: usize = 1_024;
pub const MAX_MONITOR_FOLLOW_UP_CHARS: usize = 4_000;
const MAX_MONITOR_COMMAND_CHARS: usize = 8_192;
const MAX_MONITOR_PATH_CHARS: usize = 4_096;
const MIN_MONITOR_INTERVAL_MS: u64 = 250;
const MAX_MONITOR_INTERVAL_MS: u64 = 24 * 60 * 60 * 1_000;
const MIN_MONITOR_TIMEOUT_MS: u64 = 100;
const MAX_MONITOR_TIMEOUT_MS: u64 = 7 * 24 * 60 * 60 * 1_000;

/// One strict `monitor` tool call. A single tool keeps registry operations
/// discoverable without multiplying provider declarations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum MonitorRequest {
    Register {
        source: MonitorSource,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filter: Option<MonitorFilter>,
        action: MonitorAction,
        #[serde(default)]
        occurrence: MonitorOccurrence,
        #[serde(default)]
        lifetime: MonitorLifetime,
    },
    List,
    Remove {
        monitor_id: String,
    },
}

impl MonitorRequest {
    pub fn from_tool_args(args: Value) -> ToolResult<Self> {
        let request: Self = serde_json::from_value(args).map_err(|error| {
            ToolError::invalid_argument(format!("invalid monitor arguments: {error}"))
        })?;
        request.validate()?;
        Ok(request)
    }

    fn validate(&self) -> ToolResult<()> {
        match self {
            Self::Register {
                source,
                filter,
                action,
                lifetime,
                ..
            } => {
                source.validate()?;
                if let Some(filter) = filter {
                    filter.validate(source.kind())?;
                }
                action.validate()?;
                lifetime.validate()
            }
            Self::List => Ok(()),
            Self::Remove { monitor_id } => {
                let length = monitor_id.chars().count();
                if monitor_id.trim().is_empty() || length > MAX_MONITOR_ID_CHARS {
                    return Err(ToolError::invalid_argument(format!(
                        "monitor_id must contain 1..={MAX_MONITOR_ID_CHARS} characters"
                    )));
                }
                Ok(())
            }
        }
    }
}

/// Extensible source declaration. The daemon initially activates SMS; the
/// remaining variants reserve typed, fail-closed adapter contracts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MonitorSource {
    Sms,
    Process { command: String },
    File { path: String },
    Poll { command: String, interval_ms: u64 },
    Timer { interval_ms: u64 },
}

impl MonitorSource {
    #[must_use]
    pub fn kind(&self) -> MonitorSourceKind {
        match self {
            Self::Sms => MonitorSourceKind::Sms,
            Self::Process { .. } => MonitorSourceKind::Process,
            Self::File { .. } => MonitorSourceKind::File,
            Self::Poll { .. } => MonitorSourceKind::Poll,
            Self::Timer { .. } => MonitorSourceKind::Timer,
        }
    }

    fn validate(&self) -> ToolResult<()> {
        match self {
            Self::Sms => Ok(()),
            Self::Process { command } | Self::Poll { command, .. } => {
                bounded_nonempty(command, MAX_MONITOR_COMMAND_CHARS, "monitor command")?;
                if let Self::Poll { interval_ms, .. } = self {
                    validate_interval(*interval_ms)?;
                }
                Ok(())
            }
            Self::File { path } => {
                bounded_nonempty(path, MAX_MONITOR_PATH_CHARS, "monitor file path")
            }
            Self::Timer { interval_ms } => validate_interval(*interval_ms),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MonitorSourceKind {
    Sms,
    Process,
    File,
    Poll,
    Timer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MonitorFilter {
    pub field: MonitorFilterField,
    pub operator: MonitorFilterOperator,
    pub value: String,
    #[serde(default)]
    pub case_sensitive: bool,
}

impl MonitorFilter {
    fn validate(&self, source: MonitorSourceKind) -> ToolResult<()> {
        bounded_nonempty(
            &self.value,
            MAX_MONITOR_FILTER_CHARS,
            "monitor filter value",
        )?;
        let compatible = match source {
            MonitorSourceKind::Sms => {
                matches!(
                    self.field,
                    MonitorFilterField::Address | MonitorFilterField::Body
                )
            }
            MonitorSourceKind::Process | MonitorSourceKind::File | MonitorSourceKind::Poll => {
                self.field == MonitorFilterField::Payload
            }
            MonitorSourceKind::Timer => false,
        };
        if !compatible {
            return Err(ToolError::invalid_argument(format!(
                "monitor filter field `{:?}` is not valid for source `{source:?}`",
                self.field
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MonitorFilterField {
    Address,
    Body,
    Payload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MonitorFilterOperator {
    Equals,
    Contains,
    StartsWith,
    EndsWith,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MonitorAction {
    #[serde(default = "default_report")]
    pub report: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_up: Option<String>,
}

impl MonitorAction {
    fn validate(&self) -> ToolResult<()> {
        if let Some(follow_up) = &self.follow_up {
            bounded_nonempty(follow_up, MAX_MONITOR_FOLLOW_UP_CHARS, "monitor follow_up")?;
        }
        if !self.report && self.follow_up.is_none() {
            return Err(ToolError::invalid_argument(
                "monitor action must report the event and/or provide follow_up",
            ));
        }
        Ok(())
    }
}

fn default_report() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MonitorOccurrence {
    Once,
    #[default]
    Every,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MonitorLifetime {
    #[default]
    Session,
    Timeout {
        timeout_ms: u64,
    },
}

impl MonitorLifetime {
    fn validate(self) -> ToolResult<()> {
        let Self::Timeout { timeout_ms } = self else {
            return Ok(());
        };
        if !(MIN_MONITOR_TIMEOUT_MS..=MAX_MONITOR_TIMEOUT_MS).contains(&timeout_ms) {
            return Err(ToolError::invalid_argument(format!(
                "monitor timeout_ms must be {MIN_MONITOR_TIMEOUT_MS}..={MAX_MONITOR_TIMEOUT_MS}"
            )));
        }
        Ok(())
    }
}

fn validate_interval(interval_ms: u64) -> ToolResult<()> {
    if !(MIN_MONITOR_INTERVAL_MS..=MAX_MONITOR_INTERVAL_MS).contains(&interval_ms) {
        return Err(ToolError::invalid_argument(format!(
            "monitor interval_ms must be {MIN_MONITOR_INTERVAL_MS}..={MAX_MONITOR_INTERVAL_MS}"
        )));
    }
    Ok(())
}

fn bounded_nonempty(value: &str, maximum: usize, name: &str) -> ToolResult<()> {
    let length = value.chars().count();
    if value.trim().is_empty() || length > maximum {
        return Err(ToolError::invalid_argument(format!(
            "{name} must contain 1..={maximum} characters"
        )));
    }
    Ok(())
}

pub fn monitor_manifest() -> ToolManifest {
    ToolManifest {
        name: "monitor".into(),
        description: "Register, list, or remove a durable session monitor. SMS is the active source adapter; process, file, poll, and timer are typed extension points and fail closed until activated by the daemon.".into(),
        effects: vec![],
        dispatch: DispatchMode::Await,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "operation": { "type": "string", "enum": ["register", "list", "remove"] },
                "monitor_id": { "type": "string" },
                "source": {
                    "type": "object",
                    "properties": {
                        "kind": { "type": "string", "enum": ["sms", "process", "file", "poll", "timer"] },
                        "command": { "type": "string" },
                        "path": { "type": "string" },
                        "interval_ms": { "type": "integer" }
                    },
                    "required": ["kind"]
                },
                "filter": {
                    "type": "object",
                    "properties": {
                        "field": { "type": "string", "enum": ["address", "body", "payload"] },
                        "operator": { "type": "string", "enum": ["equals", "contains", "starts_with", "ends_with"] },
                        "value": { "type": "string" },
                        "case_sensitive": { "type": "boolean" }
                    },
                    "required": ["field", "operator", "value"]
                },
                "action": {
                    "type": "object",
                    "properties": {
                        "report": { "type": "boolean" },
                        "follow_up": { "type": "string" }
                    }
                },
                "occurrence": { "type": "string", "enum": ["once", "every"] },
                "lifetime": {
                    "type": "object",
                    "properties": {
                        "kind": { "type": "string", "enum": ["session", "timeout"] },
                        "timeout_ms": { "type": "integer" }
                    },
                    "required": ["kind"]
                }
            },
            "required": ["operation"]
        }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn strict_register_list_remove_parsing() {
        let register = MonitorRequest::from_tool_args(serde_json::json!({
            "operation": "register",
            "source": {"kind": "sms"},
            "filter": {"field": "body", "operator": "contains", "value": "deploy"},
            "action": {"report": true, "follow_up": "summarize it"},
            "occurrence": "once",
            "lifetime": {"kind": "timeout", "timeout_ms": 1_000}
        }))
        .unwrap();
        assert!(matches!(register, MonitorRequest::Register { .. }));
        assert_eq!(
            MonitorRequest::from_tool_args(serde_json::json!({"operation": "list"})).unwrap(),
            MonitorRequest::List
        );
        assert!(matches!(
            MonitorRequest::from_tool_args(serde_json::json!({
                "operation": "remove", "monitor_id": "monitor-1"
            }))
            .unwrap(),
            MonitorRequest::Remove { .. }
        ));
    }

    #[test]
    fn nested_and_top_level_unknown_fields_are_rejected() {
        assert!(
            MonitorRequest::from_tool_args(serde_json::json!({
                "operation": "list", "surprise": true
            }))
            .is_err()
        );
        assert!(
            MonitorRequest::from_tool_args(serde_json::json!({
                "operation": "register",
                "source": {"kind": "sms", "surprise": true},
                "action": {"report": true}
            }))
            .is_err()
        );
    }

    #[test]
    fn source_filter_action_and_duration_cross_checks_are_enforced() {
        assert!(
            MonitorRequest::from_tool_args(serde_json::json!({
                "operation": "register",
                "source": {"kind": "sms"},
                "filter": {"field": "payload", "operator": "contains", "value": "x"},
                "action": {"report": true}
            }))
            .is_err()
        );
        assert!(
            MonitorRequest::from_tool_args(serde_json::json!({
                "operation": "register",
                "source": {"kind": "timer", "interval_ms": 249},
                "action": {"report": true}
            }))
            .is_err()
        );
        assert!(
            MonitorRequest::from_tool_args(serde_json::json!({
                "operation": "register",
                "source": {"kind": "sms"},
                "action": {"report": false}
            }))
            .is_err()
        );
    }
}
