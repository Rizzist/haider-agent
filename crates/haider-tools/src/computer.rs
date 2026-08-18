//! Native screen observation and control behind one injectable backend seam.
//!
//! Platform code returns encoded PNG bytes; the daemon owns CU-1 admission and
//! calls [`ComputerBackend::set_viewport`] with the dimensions actually sent to
//! the model. Every later coordinate is interpreted in that delivered image
//! space, which keeps Retina and CU-1 downscaling honest.

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
#[path = "computer/macos.rs"]
mod macos;

#[cfg(target_os = "linux")]
#[path = "computer/linux.rs"]
mod linux;

#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
#[path = "computer/windows.rs"]
mod windows;

use crate::broker::EffectOperation;
use crate::{ToolError, ToolResult};
use async_trait::async_trait;
use haider_protocol::computer::ComputerAction;
use haider_protocol::effect::EffectClass;
use haider_protocol::tool::{DispatchMode, ToolManifest};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub const COMPUTER_WAIT_MAX_MS: u64 = 60_000;
pub const COMPUTER_TEXT_MAX_CHARS: usize = 100_000;

/// Typed native-computer failure. Platform/TCC failures stay distinguishable
/// across the backend seam instead of collapsing into a silent empty capture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ComputerError {
    Unavailable {
        platform: String,
        message: String,
    },
    PermissionRequired {
        permission: String,
        settings_pane: String,
        settings_url: String,
        message: String,
    },
    InvalidAction {
        message: String,
    },
    Cancelled,
    Backend {
        message: String,
    },
}

impl std::fmt::Display for ComputerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable { message, .. }
            | Self::PermissionRequired { message, .. }
            | Self::InvalidAction { message }
            | Self::Backend { message } => formatter.write_str(message),
            Self::Cancelled => formatter.write_str("computer action was cancelled"),
        }
    }
}

impl std::error::Error for ComputerError {}

pub type ComputerResult<T> = Result<T, ComputerError>;

/// Cooperative cancellation owned by the effect broker. Broker close flips
/// every registered token before terminalizing an abandoned computer dispatch
/// as `Cancelled`.
#[derive(Debug, Clone, Default)]
pub struct ComputerCancelToken(Arc<AtomicBool>);

impl ComputerCancelToken {
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

    pub fn check(&self) -> ComputerResult<()> {
        if self.is_cancelled() {
            Err(ComputerError::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// Native result before the daemon performs CU-1 image admission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComputerOutput {
    ScreenshotPng(Vec<u8>),
    CursorPosition { x: u32, y: u32 },
    Confirmed { action: String },
}

/// Injectable OS boundary. Tests use a deterministic fake and never touch
/// real input devices or TCC-protected APIs.
#[async_trait]
pub trait ComputerBackend: Send + Sync {
    async fn execute(
        &self,
        action: &ComputerAction,
        cancel: &ComputerCancelToken,
    ) -> ComputerResult<ComputerOutput>;

    /// Updates the coordinate space to the exact image dimensions returned by
    /// CU-1. Backends without a screen keep the harmless default.
    fn set_viewport(&self, _width: u32, _height: u32) -> ComputerResult<()> {
        Ok(())
    }

    /// Releases any input state retained across actions. Dispatcher close
    /// invokes this on ESC before the broker records cancellation.
    async fn emergency_stop(&self) -> ComputerResult<()> {
        Ok(())
    }
}

/// Explicit portable stub, public so the unavailable law is testable even on
/// supported hosts. The platform constructor selects it only on unsupported
/// targets.
#[derive(Debug, Clone)]
pub struct UnavailableComputerBackend {
    platform: String,
}

impl UnavailableComputerBackend {
    #[must_use]
    pub fn new(platform: impl Into<String>) -> Self {
        Self {
            platform: platform.into(),
        }
    }
}

#[async_trait]
impl ComputerBackend for UnavailableComputerBackend {
    async fn execute(
        &self,
        _action: &ComputerAction,
        cancel: &ComputerCancelToken,
    ) -> ComputerResult<ComputerOutput> {
        cancel.check()?;
        Err(ComputerError::Unavailable {
            platform: self.platform.clone(),
            message: format!(
                "computer backend not available on this platform ({})",
                self.platform
            ),
        })
    }
}

/// Constructs one dispatcher-local real macOS/Linux/Windows backend or typed stub.
///
/// The state cannot be process-global: each instance retains the exact CU-1
/// viewport delivered to one turn and any mouse button held by that turn.
/// Sharing it would let one session corrupt another session's coordinate map
/// or release its button during dispatcher shutdown.
#[must_use]
pub fn platform_computer_backend() -> Arc<dyn ComputerBackend> {
    #[cfg(target_os = "macos")]
    {
        Arc::new(macos::MacOsComputerBackend::new())
    }
    #[cfg(target_os = "linux")]
    {
        Arc::new(linux::LinuxComputerBackend::new())
    }
    #[cfg(target_os = "windows")]
    {
        Arc::new(windows::WindowsComputerBackend::new())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        Arc::new(UnavailableComputerBackend::new(std::env::consts::OS))
    }
}

/// Broker-normalized dynamic computer operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComputerOperation {
    action: ComputerAction,
}

impl ComputerOperation {
    pub fn from_tool_args(arguments: Value) -> ToolResult<Self> {
        validate_argument_keys(&arguments)?;
        let action = serde_json::from_value(arguments).map_err(|error| {
            ToolError::invalid_argument(format!("invalid computer action: {error}"))
        })?;
        Self::new(action)
    }

    pub fn new(action: ComputerAction) -> ToolResult<Self> {
        validate_action(&action)?;
        Ok(Self { action })
    }

    #[must_use]
    pub fn action(&self) -> &ComputerAction {
        &self.action
    }
}

fn validate_argument_keys(arguments: &Value) -> ToolResult<()> {
    let object = arguments.as_object().ok_or_else(|| {
        ToolError::invalid_argument("computer action arguments must be a JSON object")
    })?;
    let action = object
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::invalid_argument("computer action requires string `action`"))?;
    let allowed: &[&str] = match action {
        "screenshot" | "cursor_position" | "right_click" | "middle_click" | "double_click"
        | "left_mouse_down" | "left_mouse_up" => &["action"],
        "left_click" | "mouse_move" => &["action", "x", "y"],
        "left_click_drag" => &["action", "from", "to"],
        "type" => &["action", "text"],
        "key" => &["action", "keys"],
        "scroll" => &["action", "x", "y", "direction", "amount"],
        "wait" => &["action", "ms"],
        _ => return Ok(()),
    };
    if let Some(unknown) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(ToolError::invalid_argument(format!(
            "unknown field `{unknown}` for computer action `{action}`"
        )));
    }
    Ok(())
}

impl EffectOperation for ComputerOperation {
    fn effect_class(&self) -> EffectClass {
        self.action.effect_class()
    }

    fn summary(&self) -> String {
        format!("computer {}", action_name(&self.action))
    }

    fn arguments(&self) -> ToolResult<Value> {
        serde_json::to_value(&self.action).map_err(|error| ToolError::InvalidArgument {
            message: format!("cannot encode computer action: {error}"),
        })
    }

    fn approval_preview(&self) -> Vec<String> {
        vec![match &self.action {
            ComputerAction::Type { text } => {
                #[cfg(target_os = "linux")]
                let preview = format!(
                    "Type {} character(s) into the active Linux X11 app",
                    text.chars().count()
                );
                #[cfg(target_os = "macos")]
                let preview = format!(
                    "Type {} character(s) into the active macOS app",
                    text.chars().count()
                );
                #[cfg(target_os = "windows")]
                let preview = format!(
                    "Type {} character(s) into the active Windows app",
                    text.chars().count()
                );
                #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
                let preview = format!(
                    "Type {} character(s) into the active app",
                    text.chars().count()
                );
                preview
            }
            ComputerAction::Key { keys } => format!("Press keyboard shortcut `{keys}`"),
            ComputerAction::LeftClick { x, y } | ComputerAction::MouseMove { x, y } => {
                format!("Target screenshot pixel ({x}, {y})")
            }
            ComputerAction::LeftClickDrag { from, to } => format!(
                "Drag from screenshot pixel ({}, {}) to ({}, {})",
                from.x, from.y, to.x, to.y
            ),
            ComputerAction::Scroll {
                x,
                y,
                direction,
                amount,
            } => format!("Scroll {direction:?} by {amount} at screenshot pixel ({x}, {y})"),
            ComputerAction::Wait { ms } => format!("Wait {ms} ms in a control sequence"),
            _ => self.summary(),
        }]
    }
}

fn validate_action(action: &ComputerAction) -> ToolResult<()> {
    match action {
        ComputerAction::Type { text } if text.chars().count() > COMPUTER_TEXT_MAX_CHARS => {
            Err(ToolError::invalid_argument(format!(
                "computer type text exceeds {COMPUTER_TEXT_MAX_CHARS} characters"
            )))
        }
        ComputerAction::Key { keys } if keys.trim().is_empty() => Err(ToolError::invalid_argument(
            "computer key action requires non-empty `keys`",
        )),
        ComputerAction::Scroll { amount: 0, .. } => Err(ToolError::invalid_argument(
            "computer scroll `amount` must be greater than zero",
        )),
        ComputerAction::Wait { ms } if *ms > COMPUTER_WAIT_MAX_MS => Err(
            ToolError::invalid_argument(format!("computer wait exceeds {COMPUTER_WAIT_MAX_MS} ms")),
        ),
        _ => Ok(()),
    }
}

fn action_name(action: &ComputerAction) -> &'static str {
    match action {
        ComputerAction::Screenshot => "screenshot",
        ComputerAction::CursorPosition => "cursor_position",
        ComputerAction::LeftClick { .. } => "left_click",
        ComputerAction::RightClick => "right_click",
        ComputerAction::MiddleClick => "middle_click",
        ComputerAction::DoubleClick => "double_click",
        ComputerAction::LeftMouseDown => "left_mouse_down",
        ComputerAction::LeftMouseUp => "left_mouse_up",
        ComputerAction::MouseMove { .. } => "mouse_move",
        ComputerAction::LeftClickDrag { .. } => "left_click_drag",
        ComputerAction::Type { .. } => "type",
        ComputerAction::Key { .. } => "key",
        ComputerAction::Scroll { .. } => "scroll",
        ComputerAction::Wait { .. } => "wait",
    }
}

/// Canonical provider manifest. The effect list is the static upper bound;
/// each parsed action is brokered under exactly one dynamic class.
#[must_use]
pub fn computer_manifest() -> ToolManifest {
    #[cfg(target_os = "linux")]
    let description = "Observe and control the local Linux X11 desktop. Call screenshot before cursor_position or any action with screenshot coordinates.";
    #[cfg(target_os = "macos")]
    let description = "Observe and control the local macOS desktop. Call screenshot before cursor_position or any action with screenshot coordinates.";
    #[cfg(target_os = "windows")]
    let description = "Observe and control the local Windows desktop. Call screenshot before cursor_position or any action with screenshot coordinates.";
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let description = "Observe and control the local desktop. Call screenshot before cursor_position or any action with screenshot coordinates.";
    ToolManifest {
        name: "computer".into(),
        description: description.into(),
        effects: vec![EffectClass::ScreenObserve, EffectClass::ScreenControl],
        dispatch: DispatchMode::Await,
        // Keep provider advertisement within the common OpenAI/Anthropic/
        // Gemini/Vertex schema subset. Conditional requirements and strict
        // unknown-field/range checks remain authoritative in
        // `ComputerOperation::from_tool_args`.
        input_schema: json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": [
                        "screenshot", "cursor_position", "left_click", "right_click",
                        "middle_click", "double_click", "left_mouse_down",
                        "left_mouse_up", "mouse_move", "left_click_drag", "type",
                        "key", "scroll", "wait"
                    ],
                    "description": "Computer action to perform"
                },
                "x": {"type": "integer", "description": "X pixel in the latest delivered screenshot"},
                "y": {"type": "integer", "description": "Y pixel in the latest delivered screenshot"},
                "from": {
                    "type": "object",
                    "properties": {
                        "x": {"type": "integer"},
                        "y": {"type": "integer"}
                    },
                    "required": ["x", "y"],
                    "description": "Drag start pixel in the latest delivered screenshot"
                },
                "to": {
                    "type": "object",
                    "properties": {
                        "x": {"type": "integer"},
                        "y": {"type": "integer"}
                    },
                    "required": ["x", "y"],
                    "description": "Drag end pixel in the latest delivered screenshot"
                },
                "text": {"type": "string", "description": "Text to type"},
                "keys": {"type": "string", "description": "Shortcut such as cmd+shift+4"},
                "direction": {"type": "string", "enum": ["up", "down", "left", "right"]},
                "amount": {"type": "integer", "description": "Positive scroll-line count"},
                "ms": {"type": "integer", "description": "Wait duration from 0 through 60000 milliseconds"}
            },
            "required": ["action"]
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unavailable_backend_is_typed_on_every_platform() {
        let backend = UnavailableComputerBackend::new("fixture-os");
        let error = match backend
            .execute(&ComputerAction::Screenshot, &ComputerCancelToken::new())
            .await
        {
            Ok(_) => panic!("stub must be unavailable"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            ComputerError::Unavailable { ref platform, .. } if platform == "fixture-os"
        ));
        assert_eq!(
            error.to_string(),
            "computer backend not available on this platform (fixture-os)"
        );
    }

    #[test]
    fn platform_backends_are_dispatcher_local() {
        let first = platform_computer_backend();
        let second = platform_computer_backend();
        assert!(
            !Arc::ptr_eq(&first, &second),
            "viewport and held-button state must never be shared across dispatchers"
        );
    }
}
