//! Wayland xdg-desktop-portal bridge.
//!
//! A Wayland compositor deliberately does not expose the global capture and
//! synthetic-input primitives used by the X11 backend. The companion portal
//! bridge owns one `ScreenCast` + `RemoteDesktop` session, consumes its
//! PipeWire stream, and injects through the RemoteDesktop notification API.
//! Keeping that integration in a separately packaged process keeps portal
//! and multimedia work outside the daemon while preserving a narrow, bounded
//! protocol.

use super::{ComputerBackend, ComputerCancelToken, ComputerError, ComputerOutput, ComputerResult};
use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use haider_platform::{ProcessGroup, ProcessSignal};
use haider_protocol::computer::ComputerAction;
use haider_protocol::tool::TOOL_RESULT_IMAGE_MAX_SOURCE_BYTES;
use serde_json::{Value, json};
use std::ffi::{OsStr, OsString};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

pub(crate) const WAYLAND_PORTAL_HELPER_ENV: &str = "HAIDER_WAYLAND_PORTAL_HELPER";
const PORTAL_PROTOCOL: &str = "haider-cu-wayland-v1";
const DEFAULT_HELPER: &str = "haider-wayland-portal";
const PORTAL_CONSENT_TIMEOUT: Duration = Duration::from_secs(60);
const PORTAL_ACTION_TIMEOUT: Duration = Duration::from_secs(15);
const PORTAL_STOP_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_PORTAL_RESPONSE_BYTES: usize = TOOL_RESULT_IMAGE_MAX_SOURCE_BYTES / 3 * 4 + 1_048_576;
static INPUT_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static HELD_LEFT_OWNER: Mutex<Option<u64>> = Mutex::new(None);

#[derive(Debug, Clone, Copy)]
struct Viewport {
    display_width: u32,
    display_height: u32,
    image_width: u32,
    image_height: u32,
}

#[derive(Default)]
struct MappingState {
    pending_display_size: Option<(u32, u32)>,
    viewport: Option<Viewport>,
}

struct PortalProcess {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

struct PortalChild {
    child: Child,
    group: ProcessGroup,
}

#[derive(Default)]
struct PortalState {
    process: Option<PortalProcess>,
    next_request_id: u64,
}

pub(crate) struct WaylandComputerBackend {
    input_owner: u64,
    helper: OsString,
    child: Mutex<Option<PortalChild>>,
    mapping: Mutex<MappingState>,
    portal: tokio::sync::Mutex<PortalState>,
}

impl WaylandComputerBackend {
    pub(crate) fn new(input_owner: u64) -> Self {
        Self {
            input_owner,
            helper: std::env::var_os(WAYLAND_PORTAL_HELPER_ENV)
                .filter(|value| !value.is_empty())
                .unwrap_or_else(default_helper),
            child: Mutex::new(None),
            mapping: Mutex::new(MappingState::default()),
            portal: tokio::sync::Mutex::new(PortalState::default()),
        }
    }

    fn lock_mapping(&self) -> ComputerResult<MutexGuard<'_, MappingState>> {
        self.mapping.lock().map_err(|_| ComputerError::Backend {
            message: "Linux Wayland viewport lock is poisoned".into(),
        })
    }

    fn spawn_helper(&self) -> ComputerResult<PortalProcess> {
        let mut command = Command::new(&self.helper);
        command
            .arg("serve")
            .arg("--protocol")
            .arg(PORTAL_PROTOCOL)
            .kill_on_drop(true)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        haider_platform::configure_process_group(&mut command);
        let mut child = command.spawn().map_err(|error| ComputerError::Unavailable {
            platform: "linux-wayland".into(),
            message: format!(
                "Wayland computer use requires the `{}` portal bridge ({WAYLAND_PORTAL_HELPER_ENV} overrides it): {error}; the bridge must use xdg-desktop-portal ScreenCast + RemoteDesktop/libei and the desktop will ask for interactive consent",
                self.helper.to_string_lossy()
            ),
        })?;
        let stdin = child.stdin.take().ok_or_else(|| ComputerError::Backend {
            message: "Wayland portal bridge did not expose stdin".into(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| ComputerError::Backend {
            message: "Wayland portal bridge did not expose stdout".into(),
        })?;
        let child_id = child.id().ok_or_else(|| ComputerError::Backend {
            message: "Wayland portal bridge did not expose a process id".into(),
        })?;
        let group = haider_platform::register_process_group(child_id).map_err(|error| {
            ComputerError::Backend {
                message: format!("could not register the Wayland helper process group: {error}"),
            }
        })?;
        *self.child.lock().map_err(|_| ComputerError::Backend {
            message: "Linux Wayland helper-process lock is poisoned".into(),
        })? = Some(PortalChild { child, group });
        Ok(PortalProcess {
            stdin,
            stdout: BufReader::new(stdout),
        })
    }

    async fn terminate(&self, process: &mut Option<PortalProcess>) {
        process.take();
        let child = self
            .child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(mut child) = child {
            let _ = haider_platform::signal_process_group(child.group, ProcessSignal::Kill);
            let _ = child.child.start_kill();
            match haider_platform::bounded_wait(
                "Wayland portal child reap",
                PORTAL_STOP_TIMEOUT,
                child.child.wait(),
            )
            .await
            {
                haider_platform::BoundedWait::Completed(Ok(_)) => {}
                haider_platform::BoundedWait::Completed(Err(error)) => {
                    eprintln!(
                        "haider: lifecycle event=wayland_portal_reap_failed error_kind={:?} raw_os_error={:?}",
                        error.kind(),
                        error.raw_os_error()
                    );
                }
                haider_platform::BoundedWait::TimedOut(timeout) => {
                    eprintln!(
                        "haider: lifecycle event=wayland_portal_reap_timeout timeout_ms={}",
                        timeout.limit().as_millis()
                    );
                }
            }
            haider_platform::release_process_group(child.group);
        }
        self.clear_held_left_owner();
    }

    fn clear_held_left_owner(&self) {
        let mut held = HELD_LEFT_OWNER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *held == Some(self.input_owner) {
            *held = None;
        }
    }

    async fn framed_exchange(
        process: &mut PortalProcess,
        request: &Value,
    ) -> ComputerResult<Value> {
        let encoded = serde_json::to_vec(request).map_err(|error| ComputerError::Backend {
            message: format!("could not encode Wayland portal request: {error}"),
        })?;
        let length = u64::try_from(encoded.len()).map_err(|_| ComputerError::Backend {
            message: "Wayland portal request is too large".into(),
        })?;
        process
            .stdin
            .write_all(&length.to_be_bytes())
            .await
            .map_err(portal_io("write request header"))?;
        process
            .stdin
            .write_all(&encoded)
            .await
            .map_err(portal_io("write request"))?;
        process
            .stdin
            .flush()
            .await
            .map_err(portal_io("flush request"))?;

        let mut header = [0_u8; 8];
        process
            .stdout
            .read_exact(&mut header)
            .await
            .map_err(portal_io("read response header"))?;
        let response_len =
            usize::try_from(u64::from_be_bytes(header)).map_err(|_| ComputerError::Backend {
                message: "Wayland portal bridge response length does not fit this platform".into(),
            })?;
        if response_len > MAX_PORTAL_RESPONSE_BYTES {
            return Err(ComputerError::Backend {
                message: format!(
                    "Wayland portal bridge response exceeds the {MAX_PORTAL_RESPONSE_BYTES}-byte bound"
                ),
            });
        }
        let mut encoded = vec![0_u8; response_len];
        process
            .stdout
            .read_exact(&mut encoded)
            .await
            .map_err(portal_io("read response"))?;
        serde_json::from_slice(&encoded).map_err(|error| ComputerError::Backend {
            message: format!("Wayland portal bridge returned invalid JSON: {error}"),
        })
    }

    async fn exchange(
        &self,
        command: Value,
        timeout: Duration,
        cancel: &ComputerCancelToken,
    ) -> ComputerResult<Value> {
        cancel.check()?;
        let mut state = tokio::select! {
            state = self.portal.lock() => state,
            () = wait_for_cancel(cancel) => return Err(ComputerError::Cancelled),
        };
        if state.process.is_none() {
            state.process = Some(self.spawn_helper()?);
            let request_id = state.next_request_id;
            state.next_request_id = state.next_request_id.wrapping_add(1);
            let initialize = json!({
                "protocol": PORTAL_PROTOCOL,
                "request_id": request_id,
                "command": {
                    "kind": "initialize",
                    "screen_cast": true,
                    "remote_desktop": true,
                    "input_transport": "remote_desktop_notify"
                }
            });
            let Some(process) = state.process.as_mut() else {
                return Err(portal_shape(
                    "portal process disappeared during initialization",
                ));
            };
            let initialized =
                Self::bounded_exchange(process, &initialize, PORTAL_CONSENT_TIMEOUT, cancel).await;
            if let Err(error) = initialized.and_then(|response| {
                validate_response(response, request_id).and_then(validate_initialize_output)
            }) {
                self.terminate(&mut state.process).await;
                return Err(error);
            }
        }

        let request_id = state.next_request_id;
        state.next_request_id = state.next_request_id.wrapping_add(1);
        let request = json!({
            "protocol": PORTAL_PROTOCOL,
            "request_id": request_id,
            "command": command,
        });
        let Some(process) = state.process.as_mut() else {
            return Err(portal_shape("initialized portal process disappeared"));
        };
        let response = Self::bounded_exchange(process, &request, timeout, cancel).await;
        match response.and_then(|response| validate_response(response, request_id)) {
            Ok(output) => Ok(output),
            Err(error) => {
                self.terminate(&mut state.process).await;
                Err(error)
            }
        }
    }

    async fn bounded_exchange(
        process: &mut PortalProcess,
        request: &Value,
        timeout: Duration,
        cancel: &ComputerCancelToken,
    ) -> ComputerResult<Value> {
        tokio::select! {
            result = tokio::time::timeout(timeout, Self::framed_exchange(process, request)) => {
                result.map_err(|_| ComputerError::Unavailable {
                    platform: "linux-wayland".into(),
                    message: format!(
                        "Wayland portal interaction timed out after {} seconds; ScreenCast/RemoteDesktop requires an interactive desktop consent prompt",
                        timeout.as_secs()
                    ),
                })?
            }
            () = wait_for_cancel(cancel) => Err(ComputerError::Cancelled),
        }
    }

    fn viewport_json(&self) -> ComputerResult<Value> {
        let state = self.lock_mapping()?;
        let viewport = state.viewport.ok_or_else(|| ComputerError::InvalidAction {
            message: "take a screenshot before using Wayland coordinates or input".into(),
        })?;
        Ok(json!({
            "display_width": viewport.display_width,
            "display_height": viewport.display_height,
            "image_width": viewport.image_width,
            "image_height": viewport.image_height,
        }))
    }

    fn held_left_owner() -> ComputerResult<Option<u64>> {
        HELD_LEFT_OWNER
            .lock()
            .map(|owner| *owner)
            .map_err(|_| ComputerError::Backend {
                message: "Linux Wayland input-owner lock is poisoned".into(),
            })
    }

    fn set_held_left_owner(owner: Option<u64>) -> ComputerResult<()> {
        *HELD_LEFT_OWNER.lock().map_err(|_| ComputerError::Backend {
            message: "Linux Wayland input-owner lock is poisoned".into(),
        })? = owner;
        Ok(())
    }

    async fn execute_control(
        &self,
        action: &ComputerAction,
        viewport: Value,
        cancel: &ComputerCancelToken,
    ) -> ComputerResult<ComputerOutput> {
        let _input_gate = tokio::select! {
            gate = INPUT_GATE.lock() => gate,
            () = wait_for_cancel(cancel) => return Err(ComputerError::Cancelled),
        };
        let held_owner = Self::held_left_owner()?;
        if held_owner.is_some_and(|owner| owner != self.input_owner) {
            return Err(ComputerError::Backend {
                message: "another Haider computer session currently holds the left mouse button"
                    .into(),
            });
        }
        let left_held_by_self = held_owner == Some(self.input_owner);
        if left_held_by_self
            && !matches!(
                action,
                ComputerAction::LeftMouseUp | ComputerAction::MouseMove { .. }
            )
        {
            return Err(ComputerError::InvalidAction {
                message: "release the held left mouse button before another computer action".into(),
            });
        }
        if !left_held_by_self && matches!(action, ComputerAction::LeftMouseUp) {
            return Err(ComputerError::InvalidAction {
                message: "left_mouse_up requires this computer session to hold the left button"
                    .into(),
            });
        }
        let output = self
            .exchange(
                json!({"kind": "execute", "action": action, "viewport": viewport}),
                PORTAL_ACTION_TIMEOUT,
                cancel,
            )
            .await?;
        let acknowledged = output
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| portal_shape("input response omitted action"))?;
        if acknowledged != action_name(action) {
            return Err(portal_shape(
                "input response acknowledged a different action",
            ));
        }
        match action {
            ComputerAction::LeftMouseDown => Self::set_held_left_owner(Some(self.input_owner))?,
            ComputerAction::LeftMouseUp => Self::set_held_left_owner(None)?,
            _ => {}
        }
        Ok(ComputerOutput::Confirmed {
            action: acknowledged.into(),
        })
    }
}

impl Drop for WaylandComputerBackend {
    fn drop(&mut self) {
        let child = self
            .child
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(mut child) = child {
            let _ = haider_platform::signal_process_group(child.group, ProcessSignal::Kill);
            let _ = child.child.start_kill();
            haider_platform::release_process_group(child.group);
        }
        self.clear_held_left_owner();
    }
}

fn default_helper() -> OsString {
    std::env::current_exe()
        .ok()
        .and_then(|executable| executable.parent().map(std::path::Path::to_path_buf))
        .and_then(|parent| {
            let sibling = parent.join(DEFAULT_HELPER);
            if sibling.is_file() {
                return Some(sibling);
            }
            (parent.file_name() == Some(OsStr::new("deps")))
                .then(|| parent.parent().map(|root| root.join(DEFAULT_HELPER)))
                .flatten()
                .filter(|candidate| candidate.is_file())
        })
        .map_or_else(
            || OsString::from(DEFAULT_HELPER),
            |path| path.into_os_string(),
        )
}

#[async_trait]
impl ComputerBackend for WaylandComputerBackend {
    async fn execute(
        &self,
        action: &ComputerAction,
        cancel: &ComputerCancelToken,
    ) -> ComputerResult<ComputerOutput> {
        cancel.check()?;
        if matches!(action, ComputerAction::Inspect { .. }) {
            return Err(ComputerError::InspectUnsupported {
                platform: "linux-wayland".into(),
                message: "accessibility inspection is not supported by the Wayland desktop portals"
                    .into(),
            });
        }
        if matches!(action, ComputerAction::CursorPosition) {
            return Err(ComputerError::Unavailable {
                platform: "linux-wayland".into(),
                message: "the Wayland portals do not expose an authoritative global cursor position; use the cursor embedded in a fresh screenshot".into(),
            });
        }
        let viewport = if matches!(action, ComputerAction::Screenshot) {
            Value::Null
        } else {
            self.viewport_json()?
        };
        if let ComputerAction::Wait { ms } = action {
            tokio::select! {
                () = tokio::time::sleep(Duration::from_millis(*ms)) => {
                    return Ok(ComputerOutput::Confirmed { action: "wait".into() });
                }
                () = wait_for_cancel(cancel) => return Err(ComputerError::Cancelled),
            }
        }
        match action {
            ComputerAction::Screenshot => {
                let output = self
                    .exchange(
                        json!({"kind": "execute", "action": action, "viewport": viewport}),
                        PORTAL_ACTION_TIMEOUT,
                        cancel,
                    )
                    .await?;
                let png = output
                    .get("png_base64")
                    .and_then(Value::as_str)
                    .ok_or_else(|| portal_shape("screenshot response omitted png_base64"))?;
                let width = response_dimension(&output, "width")?;
                let height = response_dimension(&output, "height")?;
                let png = BASE64.decode(png).map_err(|error| ComputerError::Backend {
                    message: format!(
                        "Wayland portal bridge returned invalid screenshot base64: {error}"
                    ),
                })?;
                self.lock_mapping()?.pending_display_size = Some((width, height));
                Ok(ComputerOutput::ScreenshotPng(png))
            }
            ComputerAction::CursorPosition | ComputerAction::Wait { .. } => unreachable!(),
            _ => self.execute_control(action, viewport, cancel).await,
        }
    }

    fn set_viewport(&self, width: u32, height: u32) -> ComputerResult<()> {
        if width == 0 || height == 0 {
            return Err(ComputerError::InvalidAction {
                message: "CU-1 returned an empty computer screenshot viewport".into(),
            });
        }
        let mut state = self.lock_mapping()?;
        let (display_width, display_height) =
            state
                .pending_display_size
                .ok_or_else(|| ComputerError::Backend {
                    message: "CU-1 viewport arrived without a matching Wayland portal capture"
                        .into(),
                })?;
        state.viewport = Some(Viewport {
            display_width,
            display_height,
            image_width: width,
            image_height: height,
        });
        Ok(())
    }

    async fn emergency_stop(&self) -> ComputerResult<()> {
        let cancel = ComputerCancelToken::new();
        let mut state = match tokio::time::timeout(PORTAL_STOP_TIMEOUT, self.portal.lock()).await {
            Ok(state) => state,
            Err(_) => {
                // Keep the kill handle outside the I/O lock so a wedged exchange cannot
                // prevent emergency stop from terminating the portal companion.
                self.terminate(&mut None).await;
                return Err(ComputerError::Unavailable {
                    platform: "linux-wayland".into(),
                    message:
                        "timed out acquiring the Wayland portal session for emergency stop; the portal companion was killed"
                            .into(),
                });
            }
        };
        if state.process.is_none() {
            self.clear_held_left_owner();
            return Ok(());
        }
        let request_id = state.next_request_id;
        state.next_request_id = state.next_request_id.wrapping_add(1);
        let request = json!({
            "protocol": PORTAL_PROTOCOL,
            "request_id": request_id,
            "command": {"kind": "emergency_stop"},
        });
        let Some(process) = state.process.as_mut() else {
            return Ok(());
        };
        let result = Self::bounded_exchange(process, &request, PORTAL_STOP_TIMEOUT, &cancel)
            .await
            .and_then(|response| validate_response(response, request_id).map(|_| ()));
        self.terminate(&mut state.process).await;
        result
    }
}

pub(crate) fn wayland_session_requested() -> bool {
    is_wayland_session(
        std::env::var_os("WAYLAND_DISPLAY").as_deref(),
        std::env::var_os("XDG_SESSION_TYPE").as_deref(),
    )
}

fn is_wayland_session(wayland_display: Option<&OsStr>, session_type: Option<&OsStr>) -> bool {
    wayland_display.is_some_and(|value| !value.is_empty())
        || session_type.is_some_and(|value| {
            value
                .to_str()
                .is_some_and(|value| value.eq_ignore_ascii_case("wayland"))
        })
}

fn validate_response(response: Value, request_id: u64) -> ComputerResult<Value> {
    if response.get("protocol").and_then(Value::as_str) != Some(PORTAL_PROTOCOL)
        || response.get("request_id").and_then(Value::as_u64) != Some(request_id)
    {
        return Err(portal_shape("response identity did not match its request"));
    }
    match response.get("status").and_then(Value::as_str) {
        Some("ok") => Ok(response.get("output").cloned().unwrap_or(Value::Null)),
        Some("denied") => Err(ComputerError::Unavailable {
            platform: "linux-wayland".into(),
            message: "Wayland ScreenCast/RemoteDesktop consent was denied; approve the interactive desktop portal prompt and retry".into(),
        }),
        Some("unavailable") => Err(ComputerError::Unavailable {
            platform: "linux-wayland".into(),
            message: format!(
                "Wayland ScreenCast/RemoteDesktop portal is unavailable: {}",
                response_message(&response)
            ),
        }),
        Some("error") => Err(ComputerError::Backend {
            message: format!("Wayland portal bridge failed: {}", response_message(&response)),
        }),
        _ => Err(portal_shape("response has an unknown status")),
    }
}

fn validate_initialize_output(output: Value) -> ComputerResult<()> {
    let capture = output.get("capture_transport").and_then(Value::as_str);
    let input = output.get("input_transport").and_then(Value::as_str);
    let pointer = output
        .get("pointer_granted")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let keyboard = output
        .get("keyboard_granted")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let stream = output.get("stream_node_id").and_then(Value::as_u64);
    let logical_width = output.get("logical_width").and_then(Value::as_u64);
    let logical_height = output.get("logical_height").and_then(Value::as_u64);
    if capture != Some("xdg-desktop-portal-screencast-pipewire")
        || input != Some("xdg-desktop-portal-remote-desktop-notify")
        || !pointer
        || !keyboard
        || stream.is_none_or(|value| value == 0 || value > u64::from(u32::MAX))
        || logical_width.is_none_or(|value| value == 0 || value > u64::from(u32::MAX))
        || logical_height.is_none_or(|value| value == 0 || value > u64::from(u32::MAX))
    {
        return Err(portal_shape(
            "initialize did not prove ScreenCast/PipeWire plus RemoteDesktop pointer and keyboard grants",
        ));
    }
    Ok(())
}

fn response_message(response: &Value) -> &str {
    response
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("no detail was provided")
}

fn response_dimension(output: &Value, field: &str) -> ComputerResult<u32> {
    let value = output
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| portal_shape(&format!("response omitted valid {field}")))?;
    Ok(value)
}

fn portal_shape(message: &str) -> ComputerError {
    ComputerError::Backend {
        message: format!("invalid Wayland portal bridge response: {message}"),
    }
}

fn portal_io(context: &'static str) -> impl FnOnce(std::io::Error) -> ComputerError {
    move |error| ComputerError::Unavailable {
        platform: "linux-wayland".into(),
        message: format!(
            "could not {context} through the Wayland portal bridge: {error}; ensure xdg-desktop-portal is running and approve its ScreenCast/RemoteDesktop consent prompt"
        ),
    }
}

fn action_name(action: &ComputerAction) -> &'static str {
    match action {
        ComputerAction::Screenshot => "screenshot",
        ComputerAction::CursorPosition => "cursor_position",
        ComputerAction::Inspect { .. } => "inspect",
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

async fn wait_for_cancel(cancel: &ComputerCancelToken) {
    while !cancel.is_cancelled() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_identity_and_denial_are_fail_closed() {
        let wrong = json!({
            "protocol": PORTAL_PROTOCOL,
            "request_id": 9,
            "status": "ok",
            "output": {}
        });
        assert!(matches!(
            validate_response(wrong, 8),
            Err(ComputerError::Backend { .. })
        ));
        let denied = json!({
            "protocol": PORTAL_PROTOCOL,
            "request_id": 8,
            "status": "denied"
        });
        assert!(matches!(
            validate_response(denied, 8),
            Err(ComputerError::Unavailable { platform, message })
                if platform == "linux-wayland" && message.contains("interactive")
        ));
    }

    #[test]
    fn portal_response_dimensions_are_bounded_to_nonzero_u32() {
        assert_eq!(response_dimension(&json!({"width": 42}), "width"), Ok(42));
        assert!(response_dimension(&json!({"width": 0}), "width").is_err());
        assert!(response_dimension(&json!({"width": u64::MAX}), "width").is_err());
    }

    #[test]
    fn initialization_requires_capture_and_both_input_grants() {
        let valid = json!({
            "capture_transport": "xdg-desktop-portal-screencast-pipewire",
            "input_transport": "xdg-desktop-portal-remote-desktop-notify",
            "pointer_granted": true,
            "keyboard_granted": true,
            "stream_node_id": 7,
            "logical_width": 1920,
            "logical_height": 1080,
        });
        assert_eq!(validate_initialize_output(valid.clone()), Ok(()));

        for field in [
            "capture_transport",
            "input_transport",
            "pointer_granted",
            "keyboard_granted",
            "stream_node_id",
            "logical_width",
            "logical_height",
        ] {
            let mut invalid = valid.clone();
            if let Some(object) = invalid.as_object_mut() {
                object.remove(field);
            } else {
                panic!("fixture must remain an object");
            }
            assert!(
                validate_initialize_output(invalid).is_err(),
                "missing {field} must fail closed"
            );
        }
        let mut zero_stream = valid;
        zero_stream["stream_node_id"] = json!(0);
        assert!(validate_initialize_output(zero_stream).is_err());
    }

    #[test]
    fn wayland_detection_is_positive_and_does_not_depend_on_display() {
        assert!(is_wayland_session(Some(OsStr::new("wayland-0")), None));
        assert!(is_wayland_session(None, Some(OsStr::new("WaYlAnD"))));
        assert!(!is_wayland_session(None, Some(OsStr::new("x11"))));
        assert!(!is_wayland_session(Some(OsStr::new("")), None));
    }
}
