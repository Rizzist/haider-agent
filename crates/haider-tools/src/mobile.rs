//! Capability-gated mobile data actions behind one injectable backend seam.

use crate::broker::EffectOperation;
use crate::{ToolError, ToolResult};
use async_trait::async_trait;
use haider_protocol::effect::EffectClass;
use haider_protocol::mobile::{MobileAction, MobileOutput};
use haider_protocol::tool::{DispatchMode, ToolManifest};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Cooperative cancellation transferred to broker ownership at dispatch.
#[derive(Debug, Clone, Default)]
pub struct MobileCancelToken(Arc<AtomicBool>);

impl MobileCancelToken {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    pub fn check(&self) -> MobileResult<()> {
        if self.is_cancelled() {
            Err(MobileError::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// Typed failure returned by a mobile backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MobileError {
    Unavailable { message: String },
    InvalidAction { message: String },
    Cancelled,
    Backend { message: String },
}

impl std::fmt::Display for MobileError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable { message }
            | Self::InvalidAction { message }
            | Self::Backend { message } => formatter.write_str(message),
            Self::Cancelled => formatter.write_str("mobile action was cancelled"),
        }
    }
}

impl std::error::Error for MobileError {}

pub type MobileResult<T> = Result<T, MobileError>;

/// Injectable mobile boundary. This lane ships only deterministic test fakes
/// and an honest unavailable production stub.
#[async_trait]
pub trait MobileBackend: Send + Sync {
    /// Performs a side-effect-free preflight. Real permission/transport gates
    /// may specialize this in later lanes.
    async fn prepare(
        &self,
        _action: &MobileAction,
        cancel: &MobileCancelToken,
    ) -> MobileResult<()> {
        cancel.check()
    }

    async fn execute(
        &self,
        action: &MobileAction,
        cancel: &MobileCancelToken,
    ) -> MobileResult<MobileOutput>;
}

/// Explicit production stub until a real mobile transport is added.
#[derive(Debug, Clone, Default)]
pub struct UnavailableMobileBackend;

#[async_trait]
impl MobileBackend for UnavailableMobileBackend {
    async fn execute(
        &self,
        _action: &MobileAction,
        cancel: &MobileCancelToken,
    ) -> MobileResult<MobileOutput> {
        cancel.check()?;
        Err(MobileError::Unavailable {
            message: "mobile backend unavailable".into(),
        })
    }
}

/// Constructs the typed unavailable backend on every host in this lane.
#[must_use]
pub fn platform_mobile_backend() -> Arc<dyn MobileBackend> {
    Arc::new(UnavailableMobileBackend)
}

/// Broker-normalized dynamic mobile operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MobileOperation {
    action: MobileAction,
}

impl MobileOperation {
    pub fn from_tool_args(arguments: Value) -> ToolResult<Self> {
        validate_argument_keys(&arguments)?;
        let action = serde_json::from_value(arguments).map_err(|error| {
            ToolError::invalid_argument(format!("invalid mobile action: {error}"))
        })?;
        Ok(Self { action })
    }

    #[must_use]
    pub fn action(&self) -> &MobileAction {
        &self.action
    }
}

fn validate_argument_keys(arguments: &Value) -> ToolResult<()> {
    let object = arguments.as_object().ok_or_else(|| {
        ToolError::invalid_argument("mobile action arguments must be a JSON object")
    })?;
    let action = object
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::invalid_argument("mobile action requires string `action`"))?;
    let allowed: &[&str] = match action {
        "sms_read" => &["action", "folder", "since", "limit"],
        _ => return Ok(()),
    };
    if let Some(unknown) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(ToolError::invalid_argument(format!(
            "unknown field `{unknown}` for mobile action `{action}`"
        )));
    }
    Ok(())
}

impl EffectOperation for MobileOperation {
    fn effect_class(&self) -> EffectClass {
        self.action.effect_class()
    }

    fn summary(&self) -> String {
        match self.action {
            MobileAction::SmsRead { .. } => "mobile sms_read".into(),
        }
    }

    fn arguments(&self) -> ToolResult<Value> {
        serde_json::to_value(&self.action).map_err(|error| ToolError::InvalidArgument {
            message: format!("cannot encode mobile action: {error}"),
        })
    }

    fn approval_preview(&self) -> Vec<String> {
        vec![match &self.action {
            MobileAction::SmsRead {
                folder,
                since,
                limit,
            } => format!(
                "Read SMS messages (folder={}, since={}, limit={})",
                folder.as_deref().unwrap_or("any"),
                since.as_deref().unwrap_or("any"),
                limit.map_or_else(|| "backend default".into(), |value| value.to_string())
            ),
        }]
    }
}

/// Canonical provider manifest. Dynamic actions remain brokered under their
/// exact effect while this list is the static upper bound.
#[must_use]
pub fn mobile_manifest() -> ToolManifest {
    ToolManifest {
        name: "mobile".into(),
        description:
            "Use an explicitly activated mobile capability. This lane supports reading SMS data."
                .into(),
        effects: vec![EffectClass::ReadSms],
        dispatch: DispatchMode::Await,
        input_schema: json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["sms_read"],
                    "description": "Mobile action to perform"
                },
                "folder": {"type": "string", "description": "Optional SMS folder such as inbox or sent"},
                "since": {"type": "string", "description": "Optional backend-defined lower time bound"},
                "limit": {"type": "integer", "description": "Optional maximum number of messages"}
            },
            "required": ["action"]
        }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn manifest_and_dynamic_effect_are_exactly_read_sms() {
        let operation = MobileOperation::from_tool_args(json!({
            "action": "sms_read",
            "folder": "inbox",
            "limit": 2
        }))
        .expect("valid operation");
        assert_eq!(operation.effect_class(), EffectClass::ReadSms);
        assert_eq!(mobile_manifest().effects, [EffectClass::ReadSms]);
    }

    #[test]
    fn operation_rejects_per_action_unknown_keys() {
        let error = MobileOperation::from_tool_args(json!({
            "action": "sms_read",
            "query": "hidden"
        }))
        .expect_err("unknown field must fail");
        assert!(error.to_string().contains("unknown field `query`"));
    }
}
