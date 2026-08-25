//! Capability-gated mobile observation, control, and data actions behind one
//! injectable backend seam.

use crate::broker::EffectOperation;
use crate::{ToolError, ToolResult};
use async_trait::async_trait;
use haider_protocol::effect::EffectClass;
use haider_protocol::mobile::{A11yNode, AppEntry, MobileAction, MobileOutput, Point4, SmsMessage};
use haider_protocol::tool::{DispatchMode, ToolManifest};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

pub const MOBILE_TEXT_MAX_CHARS: usize = 100_000;

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

/// Deterministic host-only backend used to exercise the complete mobile loop
/// without a device, transport, or APK.
#[derive(Debug, Default)]
pub struct FakeMobileBackend {
    calls: AtomicUsize,
    actions: Mutex<Vec<MobileAction>>,
}

impl FakeMobileBackend {
    #[must_use]
    pub fn call_count(&self) -> usize {
        self.calls.load(Ordering::Acquire)
    }

    pub fn actions(&self) -> MobileResult<Vec<MobileAction>> {
        self.actions
            .lock()
            .map(|actions| actions.clone())
            .map_err(|_| MobileError::Backend {
                message: "fake mobile action log is poisoned".into(),
            })
    }
}

#[async_trait]
impl MobileBackend for FakeMobileBackend {
    async fn execute(
        &self,
        action: &MobileAction,
        cancel: &MobileCancelToken,
    ) -> MobileResult<MobileOutput> {
        cancel.check()?;
        self.calls.fetch_add(1, Ordering::AcqRel);
        self.actions
            .lock()
            .map_err(|_| MobileError::Backend {
                message: "fake mobile action log is poisoned".into(),
            })?
            .push(action.clone());
        Ok(match action {
            MobileAction::Screenshot {} => MobileOutput::Screenshot(CANNED_PNG.to_vec()),
            MobileAction::A11yTree {} => MobileOutput::A11yTree(canned_a11y_tree()),
            MobileAction::Inspect { element_id, .. } => {
                let mut nodes = canned_a11y_tree();
                if let Some(element_id) = element_id {
                    nodes.retain(|node| &node.id == element_id);
                } else {
                    nodes.truncate(1);
                }
                MobileOutput::A11yTree(nodes)
            }
            MobileAction::ListApps {} => MobileOutput::AppList(vec![
                AppEntry {
                    package: "com.android.settings".into(),
                    name: "Settings".into(),
                },
                AppEntry {
                    package: "com.example.messages".into(),
                    name: "Messages".into(),
                },
            ]),
            MobileAction::SmsRead { .. } => MobileOutput::SmsList(vec![
                SmsMessage {
                    id: "sms-1".into(),
                    address: "+15550000001".into(),
                    body: "First canned message".into(),
                    date_ms: 1_725_000_000_000,
                    folder: "inbox".into(),
                },
                SmsMessage {
                    id: "sms-2".into(),
                    address: "+15550000002".into(),
                    body: "Second canned message".into(),
                    date_ms: 1_725_000_000_500,
                    folder: "inbox".into(),
                },
            ]),
            MobileAction::Tap { .. }
            | MobileAction::LongPress { .. }
            | MobileAction::Swipe { .. }
            | MobileAction::Type { .. }
            | MobileAction::Key { .. }
            | MobileAction::OpenApp { .. } => MobileOutput::Ack,
        })
    }
}

fn canned_a11y_tree() -> Vec<A11yNode> {
    vec![
        A11yNode {
            id: "root".into(),
            text: None,
            content_desc: Some("Messages screen".into()),
            class: "android.widget.FrameLayout".into(),
            resource_id: Some("com.example.messages:id/root".into()),
            bounds: Point4 {
                left: 0,
                top: 0,
                right: 1080,
                bottom: 2400,
            },
        },
        A11yNode {
            id: "compose".into(),
            text: Some("New message".into()),
            content_desc: Some("Compose".into()),
            class: "android.widget.Button".into(),
            resource_id: Some("com.example.messages:id/compose".into()),
            bounds: Point4 {
                left: 840,
                top: 2100,
                right: 1040,
                bottom: 2300,
            },
        },
        A11yNode {
            id: "thread-1".into(),
            text: Some("Example contact".into()),
            content_desc: None,
            class: "android.widget.TextView".into(),
            resource_id: Some("com.example.messages:id/thread_title".into()),
            bounds: Point4 {
                left: 40,
                top: 180,
                right: 760,
                bottom: 300,
            },
        },
    ]
}

// A complete 1x1 gray+alpha PNG. CU-1 still validates its signature, chunks,
// CRCs, dimensions, decoded allocation, and terminal IEND before CAS storage.
const CANNED_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xb5, 0x1c, 0x0c,
    0x02, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64, 0xf8, 0x0f, 0x00,
    0x01, 0x05, 0x01, 0x01, 0x27, 0x18, 0xe3, 0x66, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44,
    0xae, 0x42, 0x60, 0x82,
];

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
        Self::new(action)
    }

    pub fn new(action: MobileAction) -> ToolResult<Self> {
        validate_action(&action)?;
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
        "screenshot" | "a11y_tree" | "list_apps" => &["action"],
        "inspect" | "tap" | "long_press" => &["action", "element_id", "x", "y"],
        "swipe" => &["action", "from", "to"],
        "type" => &["action", "text"],
        "key" => &["action", "key"],
        "open_app" => &["action", "package", "name"],
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

fn validate_action(action: &MobileAction) -> ToolResult<()> {
    match action {
        MobileAction::Inspect { element_id, x, y }
        | MobileAction::Tap { element_id, x, y }
        | MobileAction::LongPress { element_id, x, y } => {
            validate_target(element_id.as_deref(), *x, *y)
        }
        MobileAction::Type { text } if text.chars().count() > MOBILE_TEXT_MAX_CHARS => {
            Err(ToolError::invalid_argument(format!(
                "mobile type text exceeds {MOBILE_TEXT_MAX_CHARS} characters"
            )))
        }
        MobileAction::OpenApp { package, name } => {
            reject_blank(package.as_deref(), "package")?;
            reject_blank(name.as_deref(), "name")?;
            if package.is_none() && name.is_none() {
                Err(ToolError::invalid_argument(
                    "mobile open_app requires non-empty `package` or `name`",
                ))
            } else {
                Ok(())
            }
        }
        _ => Ok(()),
    }
}

fn validate_target(element_id: Option<&str>, x: Option<i32>, y: Option<i32>) -> ToolResult<()> {
    reject_blank(element_id, "element_id")?;
    if x.is_some() != y.is_some() {
        return Err(ToolError::invalid_argument(
            "mobile target coordinates require both `x` and `y`",
        ));
    }
    if element_id.is_none() && x.is_none() {
        return Err(ToolError::invalid_argument(
            "mobile target requires non-empty `element_id` or both `x` and `y`",
        ));
    }
    Ok(())
}

fn reject_blank(value: Option<&str>, field: &str) -> ToolResult<()> {
    if value.is_some_and(|value| value.trim().is_empty()) {
        Err(ToolError::invalid_argument(format!(
            "mobile `{field}` must be non-empty when provided"
        )))
    } else {
        Ok(())
    }
}

impl EffectOperation for MobileOperation {
    fn effect_class(&self) -> EffectClass {
        self.action.effect_class()
    }

    fn summary(&self) -> String {
        format!("mobile {}", action_name(&self.action))
    }

    fn arguments(&self) -> ToolResult<Value> {
        serde_json::to_value(&self.action).map_err(|error| ToolError::InvalidArgument {
            message: format!("cannot encode mobile action: {error}"),
        })
    }

    fn approval_preview(&self) -> Vec<String> {
        vec![match &self.action {
            MobileAction::Type { text } => {
                format!(
                    "Type {} character(s) into the active mobile app",
                    text.chars().count()
                )
            }
            MobileAction::Key { key } => format!("Press mobile key `{key:?}`"),
            MobileAction::Inspect { element_id, x, y }
            | MobileAction::Tap { element_id, x, y }
            | MobileAction::LongPress { element_id, x, y } => element_id.as_ref().map_or_else(
                || {
                    format!(
                        "Target mobile pixel ({}, {})",
                        x.unwrap_or_default(),
                        y.unwrap_or_default()
                    )
                },
                |element_id| format!("Target mobile element `{element_id}`"),
            ),
            MobileAction::Swipe { from, to } => format!(
                "Swipe from mobile pixel ({}, {}) to ({}, {})",
                from.x, from.y, to.x, to.y
            ),
            MobileAction::OpenApp { package, name } => format!(
                "Open mobile app {}",
                package.as_deref().or(name.as_deref()).unwrap_or("unknown")
            ),
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
            _ => self.summary(),
        }]
    }
}

fn action_name(action: &MobileAction) -> &'static str {
    match action {
        MobileAction::Screenshot {} => "screenshot",
        MobileAction::A11yTree {} => "a11y_tree",
        MobileAction::Inspect { .. } => "inspect",
        MobileAction::Tap { .. } => "tap",
        MobileAction::LongPress { .. } => "long_press",
        MobileAction::Swipe { .. } => "swipe",
        MobileAction::Type { .. } => "type",
        MobileAction::Key { .. } => "key",
        MobileAction::OpenApp { .. } => "open_app",
        MobileAction::ListApps {} => "list_apps",
        MobileAction::SmsRead { .. } => "sms_read",
    }
}

/// Canonical provider manifest. Dynamic actions remain brokered under their
/// exact effect while this list is the static upper bound.
#[must_use]
pub fn mobile_manifest() -> ToolManifest {
    ToolManifest {
        name: "mobile".into(),
        description: "Observe and control an explicitly activated mobile capability, list apps, or read SMS data. Call screenshot or a11y_tree before coordinate or element actions.".into(),
        effects: vec![
            EffectClass::ReadSms,
            EffectClass::MobileObserve,
            EffectClass::MobileControl,
        ],
        dispatch: DispatchMode::Await,
        // Conditional requirements and strict unknown-field/range checks are
        // authoritative in `MobileOperation::from_tool_args`.
        input_schema: json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": [
                        "screenshot", "a11y_tree", "inspect", "tap", "long_press", "swipe",
                        "type", "key", "open_app", "list_apps", "sms_read"
                    ],
                    "description": "Mobile action to perform"
                },
                "element_id": {"type": "string", "description": "Accessibility element id"},
                "x": {"type": "integer", "description": "X pixel in the latest mobile screenshot"},
                "y": {"type": "integer", "description": "Y pixel in the latest mobile screenshot"},
                "from": {
                    "type": "object",
                    "properties": {"x": {"type": "integer"}, "y": {"type": "integer"}},
                    "required": ["x", "y"],
                    "description": "Swipe start pixel"
                },
                "to": {
                    "type": "object",
                    "properties": {"x": {"type": "integer"}, "y": {"type": "integer"}},
                    "required": ["x", "y"],
                    "description": "Swipe end pixel"
                },
                "text": {"type": "string", "description": "Text to type"},
                "key": {"type": "string", "enum": ["back", "home", "enter", "recents"]},
                "package": {"type": "string", "description": "Android application package"},
                "name": {"type": "string", "description": "Installed application name"},
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
    use haider_protocol::mobile::{MobileKey, Point};

    #[test]
    fn parser_covers_every_action_and_effect_exactly() {
        let cases = [
            (json!({"action": "screenshot"}), EffectClass::MobileObserve),
            (json!({"action": "a11y_tree"}), EffectClass::MobileObserve),
            (
                json!({"action": "inspect", "element_id": "compose"}),
                EffectClass::MobileObserve,
            ),
            (
                json!({"action": "tap", "x": 10, "y": 20}),
                EffectClass::MobileControl,
            ),
            (
                json!({"action": "long_press", "element_id": "compose"}),
                EffectClass::MobileControl,
            ),
            (
                json!({"action": "swipe", "from": {"x": 0, "y": 1}, "to": {"x": 2, "y": 3}}),
                EffectClass::MobileControl,
            ),
            (
                json!({"action": "type", "text": "hello"}),
                EffectClass::MobileControl,
            ),
            (
                json!({"action": "key", "key": "back"}),
                EffectClass::MobileControl,
            ),
            (
                json!({"action": "open_app", "package": "com.example"}),
                EffectClass::MobileControl,
            ),
            (json!({"action": "list_apps"}), EffectClass::MobileObserve),
            (
                json!({"action": "sms_read", "limit": 2}),
                EffectClass::ReadSms,
            ),
        ];
        for (arguments, effect) in cases {
            let mut with_unknown = arguments.clone();
            with_unknown
                .as_object_mut()
                .expect("fixture action object")
                .insert("future".into(), Value::Bool(true));
            assert_eq!(
                MobileOperation::from_tool_args(arguments)
                    .expect("valid mobile action")
                    .effect_class(),
                effect
            );
            assert!(
                MobileOperation::from_tool_args(with_unknown).is_err(),
                "every action must reject an unknown top-level field"
            );
        }
        assert_eq!(
            mobile_manifest().effects,
            [
                EffectClass::ReadSms,
                EffectClass::MobileObserve,
                EffectClass::MobileControl
            ]
        );
    }

    #[test]
    fn parser_rejects_unknown_fields_and_invalid_runtime_shapes() {
        assert!(
            MobileOperation::from_tool_args(json!({
                "action": "swipe",
                "from": {"x": 1, "y": 2, "z": 3},
                "to": {"x": 3, "y": 4}
            }))
            .is_err()
        );
        for arguments in [
            json!({"action": "tap"}),
            json!({"action": "long_press", "element_id": "  "}),
            json!({"action": "tap", "element_id": "  ", "x": 1, "y": 2}),
            json!({"action": "inspect", "x": 1}),
            json!({"action": "key", "key": ""}),
            json!({"action": "open_app", "package": "", "name": "  "}),
            json!({"action": "open_app", "package": "  ", "name": "Settings"}),
        ] {
            assert!(MobileOperation::from_tool_args(arguments).is_err());
        }
        let oversized = "x".repeat(MOBILE_TEXT_MAX_CHARS + 1);
        assert!(
            MobileOperation::from_tool_args(json!({"action": "type", "text": oversized})).is_err()
        );
    }

    #[tokio::test]
    async fn fake_backend_returns_canned_observations_and_acks() {
        let backend = FakeMobileBackend::default();
        let cancel = MobileCancelToken::new();
        assert!(
            matches!(backend.execute(&MobileAction::Screenshot {}, &cancel).await, Ok(MobileOutput::Screenshot(bytes)) if !bytes.is_empty())
        );
        assert!(
            matches!(backend.execute(&MobileAction::A11yTree {}, &cancel).await, Ok(MobileOutput::A11yTree(nodes)) if nodes.len() == 3)
        );
        assert!(
            matches!(backend.execute(&MobileAction::ListApps {}, &cancel).await, Ok(MobileOutput::AppList(apps)) if apps.len() == 2)
        );
        assert_eq!(
            backend
                .execute(
                    &MobileAction::Swipe {
                        from: Point { x: 0, y: 0 },
                        to: Point { x: 0, y: 100 },
                    },
                    &cancel,
                )
                .await
                .expect("fake swipe"),
            MobileOutput::Ack
        );
        assert_eq!(
            backend
                .execute(
                    &MobileAction::Key {
                        key: MobileKey::Home
                    },
                    &cancel
                )
                .await
                .expect("fake key"),
            MobileOutput::Ack
        );
        assert_eq!(backend.call_count(), 5);
    }
}
