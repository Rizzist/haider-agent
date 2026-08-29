//! Linux companion for the daemon's bounded Wayland portal protocol.

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("haider-wayland-portal is available only on Linux");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
fn main() {
    if let Err(error) = linux::run() {
        eprintln!("haider-wayland-portal: {error}");
        std::process::exit(1);
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use futures_util::StreamExt as _;
    use haider_protocol::computer::{ComputerAction, ScreenPoint, ScrollDirection};
    use haider_protocol::tool::TOOL_RESULT_IMAGE_MAX_SOURCE_BYTES;
    use rustix::io::{FdFlags, fcntl_setfd};
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    use zbus::zvariant::{
        Array, ObjectPath, OwnedObjectPath, OwnedValue, Structure, Value as DbusValue,
    };

    const PROTOCOL: &str = "haider-cu-wayland-v1";
    const MAX_REQUEST_BYTES: usize = 1_048_576;
    const PORTAL_DESTINATION: &str = "org.freedesktop.portal.Desktop";
    const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
    const REMOTE_DESKTOP: &str = "org.freedesktop.portal.RemoteDesktop";
    const SCREEN_CAST: &str = "org.freedesktop.portal.ScreenCast";
    const REQUEST: &str = "org.freedesktop.portal.Request";
    const SESSION: &str = "org.freedesktop.portal.Session";
    const DEVICE_KEYBOARD: u32 = 1;
    const DEVICE_POINTER: u32 = 2;
    const SOURCE_MONITOR: u32 = 1;
    const CURSOR_EMBEDDED: u32 = 2;
    const BUTTON_LEFT: i32 = 0x110;
    const BUTTON_RIGHT: i32 = 0x111;
    const BUTTON_MIDDLE: i32 = 0x112;
    const STATE_RELEASED: u32 = 0;
    const STATE_PRESSED: u32 = 1;
    const METHOD_TIMEOUT: Duration = Duration::from_secs(5);
    const CONSENT_TIMEOUT: Duration = Duration::from_secs(55);
    const CAPTURE_TIMEOUT: Duration = Duration::from_secs(12);

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum FailureKind {
        Denied,
        Unavailable,
        Error,
    }

    #[derive(Debug)]
    pub(super) struct HelperError {
        kind: FailureKind,
        message: String,
    }

    impl HelperError {
        fn unavailable(message: impl Into<String>) -> Self {
            Self {
                kind: FailureKind::Unavailable,
                message: message.into(),
            }
        }

        fn error(message: impl Into<String>) -> Self {
            Self {
                kind: FailureKind::Error,
                message: message.into(),
            }
        }
    }

    impl std::fmt::Display for HelperError {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str(&self.message)
        }
    }

    struct Portal {
        connection: zbus::Connection,
        sender_component: String,
        session_handle: Option<String>,
        stream_node: Option<u32>,
        logical_size: Option<(u32, u32)>,
        next_token: u64,
        left_button_down: bool,
    }

    impl Portal {
        async fn connect() -> Result<Self, HelperError> {
            let connection = zbus::connection::Builder::session()
                .map_err(|error| {
                    bus_error(
                        FailureKind::Unavailable,
                        "connect to the D-Bus session bus",
                        error,
                    )
                })?
                .method_timeout(METHOD_TIMEOUT)
                .build()
                .await
                .map_err(|error| {
                    bus_error(
                        FailureKind::Unavailable,
                        "connect to the D-Bus session bus",
                        error,
                    )
                })?;
            let unique = connection.unique_name().ok_or_else(|| {
                HelperError::unavailable("D-Bus session connection has no unique name")
            })?;
            let sender_component = unique
                .as_str()
                .trim_start_matches(':')
                .replace(|character: char| !character.is_ascii_alphanumeric(), "_");
            Ok(Self {
                connection,
                sender_component,
                session_handle: None,
                stream_node: None,
                logical_size: None,
                next_token: 1,
                left_button_down: false,
            })
        }

        async fn initialize(&mut self) -> Result<Value, HelperError> {
            let create_token = self.token("create");
            let session_token = self.token("session");
            let create_options = HashMap::from([
                ("handle_token", DbusValue::from(create_token.as_str())),
                (
                    "session_handle_token",
                    DbusValue::from(session_token.as_str()),
                ),
            ]);
            let create = self
                .portal_request(
                    REMOTE_DESKTOP,
                    "CreateSession",
                    &(create_options,),
                    &create_token,
                )
                .await?;
            let session_handle = result_string(
                &create,
                "session_handle",
                "CreateSession omitted session_handle",
            )?;
            self.session_handle = Some(session_handle.clone());

            let devices_token = self.token("devices");
            let device_options = HashMap::from([
                ("handle_token", DbusValue::from(devices_token.as_str())),
                ("types", DbusValue::from(DEVICE_POINTER | DEVICE_KEYBOARD)),
            ]);
            let session_path = object_path(&session_handle)?;
            self.portal_request(
                REMOTE_DESKTOP,
                "SelectDevices",
                &(session_path.clone(), device_options),
                &devices_token,
            )
            .await?;

            let sources_token = self.token("sources");
            let source_options = HashMap::from([
                ("handle_token", DbusValue::from(sources_token.as_str())),
                ("types", DbusValue::from(SOURCE_MONITOR)),
                ("multiple", DbusValue::from(false)),
                ("cursor_mode", DbusValue::from(CURSOR_EMBEDDED)),
            ]);
            self.portal_request(
                SCREEN_CAST,
                "SelectSources",
                &(session_path.clone(), source_options),
                &sources_token,
            )
            .await?;

            let start_token = self.token("start");
            let start_options =
                HashMap::from([("handle_token", DbusValue::from(start_token.as_str()))]);
            let started = self
                .portal_request(
                    REMOTE_DESKTOP,
                    "Start",
                    &(session_path, "", start_options),
                    &start_token,
                )
                .await?;
            let devices = result_u32(&started, "devices", "RemoteDesktop.Start omitted devices")?;
            if devices & (DEVICE_POINTER | DEVICE_KEYBOARD) != DEVICE_POINTER | DEVICE_KEYBOARD {
                return Err(HelperError {
                    kind: FailureKind::Denied,
                    message:
                        "interactive portal consent did not grant pointer and keyboard control"
                            .into(),
                });
            }
            let streams_value = started.get("streams").ok_or_else(|| {
                HelperError::error("RemoteDesktop.Start omitted ScreenCast streams")
            })?;
            let streams = match <&Array<'_>>::try_from(streams_value) {
                Ok(streams) => streams,
                Err(_) if dbus_value_has_children(streams_value) => {
                    return Err(HelperError::error(
                        "ScreenCast streams has an invalid container type",
                    ));
                }
                Err(_) => {
                    return Err(HelperError {
                        kind: FailureKind::Denied,
                        message: "interactive portal consent did not grant a monitor stream".into(),
                    });
                }
            };
            let stream = streams.inner().first().ok_or_else(|| HelperError {
                kind: FailureKind::Denied,
                message: "interactive portal consent did not grant a monitor stream".into(),
            })?;
            let stream = <&Structure<'_>>::try_from(stream)
                .map_err(|_| HelperError::error("portal response is missing a tuple child"))?;
            let node = stream
                .fields()
                .first()
                .ok_or_else(|| HelperError::error("portal response is missing a tuple child"))
                .and_then(|value| {
                    u32::try_from(value)
                        .map_err(|_| HelperError::error("portal value is not uint32"))
                })?;
            let properties: HashMap<String, OwnedValue> = stream
                .fields()
                .get(1)
                .ok_or_else(|| HelperError::error("portal response is missing a tuple child"))?
                .try_clone()
                .and_then(TryInto::try_into)
                .map_err(|_| HelperError::error("ScreenCast stream has an invalid type"))?;
            let size = properties
                .get("logical_size")
                .or_else(|| properties.get("size"))
                .ok_or_else(|| HelperError::error("ScreenCast stream omitted logical size"))?;
            let (logical_width, logical_height) = positive_dimensions(size)?;
            self.stream_node = Some(node);
            self.logical_size = Some((logical_width, logical_height));
            Ok(json!({
                "capture_transport": "xdg-desktop-portal-screencast-pipewire",
                "input_transport": "xdg-desktop-portal-remote-desktop-notify",
                "pointer_granted": true,
                "keyboard_granted": true,
                "stream_node_id": node,
                "logical_width": logical_width,
                "logical_height": logical_height,
            }))
        }

        async fn execute(&mut self, command: &Value) -> Result<Value, HelperError> {
            let action: ComputerAction = serde_json::from_value(
                command
                    .get("action")
                    .cloned()
                    .ok_or_else(|| HelperError::error("execute omitted action"))?,
            )
            .map_err(|error| HelperError::error(format!("invalid computer action: {error}")))?;
            if matches!(action, ComputerAction::Screenshot) {
                return self.screenshot().await;
            }
            if matches!(
                action,
                ComputerAction::CursorPosition | ComputerAction::Inspect { .. }
            ) {
                return Err(HelperError::unavailable(
                    "the Wayland portal helper does not expose accessibility or global cursor queries",
                ));
            }
            let viewport = command
                .get("viewport")
                .ok_or_else(|| HelperError::error("input action omitted viewport"))?;
            self.input(&action, viewport).await?;
            Ok(json!({"action": action_name(&action)}))
        }

        async fn screenshot(&mut self) -> Result<Value, HelperError> {
            let node = self
                .stream_node
                .ok_or_else(|| HelperError::error("ScreenCast stream is not initialized"))?;
            let (logical_width, logical_height) = self
                .logical_size
                .ok_or_else(|| HelperError::error("ScreenCast logical size is unavailable"))?;
            let remote = self.open_pipewire_remote().await?;
            fcntl_setfd(&remote, FdFlags::empty()).map_err(|error| {
                HelperError::unavailable(format!(
                    "could not make the portal PipeWire descriptor inheritable: {error}"
                ))
            })?;
            let fd = remote.as_raw_fd();
            let fd_argument = format!("fd={fd}");
            let path_argument = format!("path={node}");
            let mut capture = Command::new("gst-launch-1.0")
                .args([
                    "-q",
                    "pipewiresrc",
                    fd_argument.as_str(),
                    path_argument.as_str(),
                    "num-buffers=1",
                    "!",
                    "videoconvert",
                    "!",
                    "pngenc",
                    "snapshot=true",
                    "!",
                    "fdsink",
                    "fd=1",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|error| HelperError::unavailable(format!(
                    "could not start GStreamer PipeWire capture: {error}; install gstreamer1.0-tools, gstreamer1.0-pipewire, gstreamer1.0-plugins-base, and gstreamer1.0-plugins-good"
                )))?;
            let Some(stdout) = capture.stdout.take() else {
                let _ = capture.kill();
                let _ = capture.wait();
                return Err(HelperError::error(
                    "GStreamer capture did not expose its PNG pipe",
                ));
            };
            let reader = std::thread::spawn(move || {
                let mut png = Vec::new();
                stdout
                    .take(TOOL_RESULT_IMAGE_MAX_SOURCE_BYTES as u64 + 1)
                    .read_to_end(&mut png)
                    .map(|_| png)
            });
            let deadline = Instant::now() + CAPTURE_TIMEOUT;
            let status = loop {
                match capture.try_wait() {
                    Ok(Some(status)) => break status,
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Ok(None) => {
                        let _ = capture.kill();
                        let _ = capture.wait();
                        let _ = reader.join();
                        return Err(HelperError::unavailable(format!(
                            "GStreamer PipeWire capture timed out after {} seconds",
                            CAPTURE_TIMEOUT.as_secs()
                        )));
                    }
                    Err(error) => {
                        let _ = capture.kill();
                        let _ = capture.wait();
                        let _ = reader.join();
                        return Err(HelperError::error(format!(
                            "could not poll GStreamer PipeWire capture: {error}"
                        )));
                    }
                }
            };
            if !status.success() {
                let _ = reader.join();
                return Err(HelperError::unavailable(format!(
                    "GStreamer PipeWire capture exited with {status}; verify the PipeWire and base plugins are installed and portal consent is still active"
                )));
            }
            let png = reader
                .join()
                .map_err(|_| HelperError::error("GStreamer PNG reader panicked"))?
                .map_err(|error| HelperError::error(format!("read GStreamer PNG pipe: {error}")))?;
            if png.len() > TOOL_RESULT_IMAGE_MAX_SOURCE_BYTES {
                return Err(HelperError::error(
                    "portal screenshot grew beyond its source bound",
                ));
            }
            if !png.starts_with(b"\x89PNG\r\n\x1a\n") {
                return Err(HelperError::error(
                    "GStreamer PipeWire capture did not return a PNG",
                ));
            }
            Ok(json!({
                "png_base64": BASE64.encode(png),
                "width": logical_width,
                "height": logical_height,
            }))
        }

        async fn input(
            &mut self,
            action: &ComputerAction,
            viewport: &Value,
        ) -> Result<(), HelperError> {
            match action {
                ComputerAction::LeftClick { x, y } => {
                    self.pointer_absolute(map_point(*x, *y, viewport)?).await?;
                    self.button(BUTTON_LEFT, true).await?;
                    self.button(BUTTON_LEFT, false).await
                }
                ComputerAction::RightClick => {
                    self.button(BUTTON_RIGHT, true).await?;
                    self.button(BUTTON_RIGHT, false).await
                }
                ComputerAction::MiddleClick => {
                    self.button(BUTTON_MIDDLE, true).await?;
                    self.button(BUTTON_MIDDLE, false).await
                }
                ComputerAction::DoubleClick => {
                    for _ in 0..2 {
                        self.button(BUTTON_LEFT, true).await?;
                        self.button(BUTTON_LEFT, false).await?;
                    }
                    Ok(())
                }
                ComputerAction::LeftMouseDown => {
                    if self.left_button_down {
                        return Err(HelperError::error("left mouse button is already held"));
                    }
                    self.button(BUTTON_LEFT, true).await?;
                    self.left_button_down = true;
                    Ok(())
                }
                ComputerAction::LeftMouseUp => {
                    if !self.left_button_down {
                        return Err(HelperError::error("left mouse button is not held"));
                    }
                    self.button(BUTTON_LEFT, false).await?;
                    self.left_button_down = false;
                    Ok(())
                }
                ComputerAction::MouseMove { x, y } => {
                    self.pointer_absolute(map_point(*x, *y, viewport)?).await
                }
                ComputerAction::LeftClickDrag { from, to } => {
                    self.pointer_absolute(map_screen_point(*from, viewport)?)
                        .await?;
                    self.button(BUTTON_LEFT, true).await?;
                    self.pointer_absolute(map_screen_point(*to, viewport)?)
                        .await?;
                    self.button(BUTTON_LEFT, false).await
                }
                ComputerAction::Type { text } => {
                    for scalar in text.chars() {
                        self.keysym(keysym_for_char(scalar), true).await?;
                        self.keysym(keysym_for_char(scalar), false).await?;
                    }
                    Ok(())
                }
                ComputerAction::Key { keys } => self.key_chord(keys).await,
                ComputerAction::Scroll {
                    x,
                    y,
                    direction,
                    amount,
                } => {
                    self.pointer_absolute(map_point(*x, *y, viewport)?).await?;
                    let steps = i32::try_from(*amount).map_err(|_| {
                        HelperError::error("scroll amount exceeds the portal int32 step range")
                    })?;
                    let (axis, steps) = match direction {
                        ScrollDirection::Up => (0, -steps),
                        ScrollDirection::Down => (0, steps),
                        ScrollDirection::Left => (1, -steps),
                        ScrollDirection::Right => (1, steps),
                    };
                    self.pointer_axis_discrete(axis, steps).await
                }
                ComputerAction::Wait { .. } => Ok(()),
                ComputerAction::Screenshot
                | ComputerAction::CursorPosition
                | ComputerAction::Inspect { .. } => Err(HelperError::error(
                    "observe action reached the Wayland input dispatcher",
                )),
            }
        }

        async fn pointer_absolute(&self, point: (f64, f64)) -> Result<(), HelperError> {
            let session = self.session()?;
            let stream = self
                .stream_node
                .ok_or_else(|| HelperError::error("no stream"))?;
            self.call_empty(
                REMOTE_DESKTOP,
                "NotifyPointerMotionAbsolute",
                &(session, empty_options(), stream, point.0, point.1),
            )
            .await
        }

        async fn button(&self, button: i32, pressed: bool) -> Result<(), HelperError> {
            let session = self.session()?;
            let state = if pressed {
                STATE_PRESSED
            } else {
                STATE_RELEASED
            };
            self.call_empty(
                REMOTE_DESKTOP,
                "NotifyPointerButton",
                &(session, empty_options(), button, state),
            )
            .await
        }

        async fn pointer_axis_discrete(&self, axis: u32, steps: i32) -> Result<(), HelperError> {
            let session = self.session()?;
            self.call_empty(
                REMOTE_DESKTOP,
                "NotifyPointerAxisDiscrete",
                &(session, empty_options(), axis, steps),
            )
            .await
        }

        async fn keysym(&self, keysym: u32, pressed: bool) -> Result<(), HelperError> {
            let session = self.session()?;
            let keysym = i32::try_from(keysym)
                .map_err(|_| HelperError::error("keysym exceeds portal int32 range"))?;
            let state = if pressed {
                STATE_PRESSED
            } else {
                STATE_RELEASED
            };
            self.call_empty(
                REMOTE_DESKTOP,
                "NotifyKeyboardKeysym",
                &(session, empty_options(), keysym, state),
            )
            .await
        }

        async fn key_chord(&self, chord: &str) -> Result<(), HelperError> {
            let mut parts = chord.split('+').collect::<Vec<_>>();
            let key = parts
                .pop()
                .filter(|key| !key.is_empty())
                .ok_or_else(|| HelperError::error("key chord omitted its main key"))?;
            let modifiers = parts
                .into_iter()
                .map(|modifier| match modifier.to_ascii_lowercase().as_str() {
                    "cmd" | "command" | "meta" | "super" => Ok(0xffeb),
                    "shift" => Ok(0xffe1),
                    "ctrl" | "control" => Ok(0xffe3),
                    "alt" | "option" => Ok(0xffe9),
                    unknown => Err(HelperError::error(format!(
                        "unsupported Wayland key modifier `{unknown}`"
                    ))),
                })
                .collect::<Result<Vec<_>, _>>()?;
            let main = keysym_for_key_name(key)
                .ok_or_else(|| HelperError::error(format!("unsupported Wayland key `{key}`")))?;
            for modifier in &modifiers {
                self.keysym(*modifier, true).await?;
            }
            let result = match self.keysym(main, true).await {
                Ok(()) => self.keysym(main, false).await,
                Err(error) => Err(error),
            };
            for modifier in modifiers.iter().rev() {
                self.keysym(*modifier, false).await?;
            }
            result
        }

        async fn emergency_stop(&mut self) -> Result<Value, HelperError> {
            if self.left_button_down {
                self.button(BUTTON_LEFT, false).await?;
                self.left_button_down = false;
            }
            self.close_session().await;
            Ok(json!({"stopped": true}))
        }

        async fn open_pipewire_remote(&self) -> Result<OwnedFd, HelperError> {
            let session = self.session()?;
            let reply = self
                .call(
                    PORTAL_PATH,
                    SCREEN_CAST,
                    "OpenPipeWireRemote",
                    &(session, empty_options()),
                )
                .await
                .map_err(|error| HelperError {
                    kind: FailureKind::Unavailable,
                    message: error.message.replace(
                        "call portal method org.freedesktop.portal.ScreenCast.OpenPipeWireRemote",
                        "open the ScreenCast PipeWire remote",
                    ),
                })?;
            let fd: zbus::zvariant::OwnedFd = reply.body().deserialize().map_err(|error| {
                bus_error(
                    FailureKind::Unavailable,
                    "receive the ScreenCast PipeWire descriptor",
                    error,
                )
            })?;
            Ok(fd.into())
        }

        async fn portal_request<B>(
            &self,
            interface: &str,
            method: &str,
            body: &B,
            token: &str,
        ) -> Result<HashMap<String, OwnedValue>, HelperError>
        where
            B: serde::Serialize + zbus::zvariant::DynamicType,
        {
            let request_path = format!(
                "/org/freedesktop/portal/desktop/request/{}/{}",
                self.sender_component, token
            );
            let request_proxy = zbus::Proxy::new(
                &self.connection,
                PORTAL_DESTINATION,
                request_path.as_str(),
                REQUEST,
            )
            .await
            .map_err(|error| {
                bus_error(
                    FailureKind::Unavailable,
                    "subscribe to the portal response signal",
                    error,
                )
            })?;
            let mut responses =
                request_proxy
                    .receive_signal("Response")
                    .await
                    .map_err(|error| {
                        bus_error(
                            FailureKind::Unavailable,
                            "subscribe to the portal response signal",
                            error,
                        )
                    })?;
            let reply = self.call(PORTAL_PATH, interface, method, body).await?;
            let (returned_path,): (OwnedObjectPath,) =
                reply.body().deserialize().map_err(|error| {
                    bus_error(FailureKind::Error, "decode the portal request path", error)
                })?;
            if returned_path.as_str() != request_path {
                return Err(HelperError::error(
                    "portal returned a request path that did not match handle_token",
                ));
            }
            let response_message = tokio::time::timeout(CONSENT_TIMEOUT, responses.next())
                .await
                .map_err(|_| {
                    HelperError::unavailable(
                        "portal interaction timed out waiting for interactive consent",
                    )
                })?
                .ok_or_else(|| {
                    HelperError::unavailable(
                        "portal response signal stream ended before interactive consent completed",
                    )
                })?;
            let (response, results): (u32, HashMap<String, OwnedValue>) =
                response_message.body().deserialize().map_err(|error| {
                    bus_error(FailureKind::Error, "decode the portal response", error)
                })?;
            match response {
                0 => Ok(results),
                1 => Err(HelperError {
                    kind: FailureKind::Denied,
                    message: "interactive portal consent was cancelled".into(),
                }),
                _ => Err(HelperError {
                    kind: FailureKind::Denied,
                    message: "interactive portal consent was denied".into(),
                }),
            }
        }

        async fn call<B>(
            &self,
            path: &str,
            interface: &str,
            method: &str,
            body: &B,
        ) -> Result<zbus::Message, HelperError>
        where
            B: serde::Serialize + zbus::zvariant::DynamicType,
        {
            self.connection
                .call_method(
                    Some(PORTAL_DESTINATION),
                    path,
                    Some(interface),
                    method,
                    body,
                )
                .await
                .map_err(|error| {
                    bus_error(
                        FailureKind::Unavailable,
                        &format!("call portal method {interface}.{method}"),
                        error,
                    )
                })
        }

        async fn call_empty<B>(
            &self,
            interface: &str,
            method: &str,
            body: &B,
        ) -> Result<(), HelperError>
        where
            B: serde::Serialize + zbus::zvariant::DynamicType,
        {
            drop(self.call(PORTAL_PATH, interface, method, body).await?);
            Ok(())
        }

        async fn close_session(&mut self) {
            let Some(session) = self.session_handle.take() else {
                return;
            };
            let _ = self.call(session.as_str(), SESSION, "Close", &()).await;
            self.stream_node = None;
            self.logical_size = None;
        }

        fn session(&self) -> Result<OwnedObjectPath, HelperError> {
            let session = self
                .session_handle
                .as_deref()
                .ok_or_else(|| HelperError::error("portal session is not initialized"))?;
            object_path(session)
        }

        fn token(&mut self, prefix: &str) -> String {
            let token = format!("haider_{prefix}_{}_{}", std::process::id(), self.next_token);
            self.next_token = self.next_token.wrapping_add(1);
            token
        }
    }

    pub(super) fn run() -> Result<(), HelperError> {
        let args = std::env::args().skip(1).collect::<Vec<_>>();
        if args.as_slice() != ["serve", "--protocol", PROTOCOL] {
            return Err(HelperError::error(
                "usage: haider-wayland-portal serve --protocol haider-cu-wayland-v1",
            ));
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| {
                HelperError::error(format!("could not start the portal runtime: {error}"))
            })?;
        let mut portal = runtime.block_on(Portal::connect())?;
        let result = serve(&runtime, &mut portal);
        runtime.block_on(portal.close_session());
        result
    }

    fn serve(runtime: &tokio::runtime::Runtime, portal: &mut Portal) -> Result<(), HelperError> {
        let mut input = std::io::stdin().lock();
        let mut output = std::io::stdout().lock();
        loop {
            let mut header = [0_u8; 8];
            match input.read_exact(&mut header) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(error) => {
                    return Err(HelperError::error(format!(
                        "could not read request header: {error}"
                    )));
                }
            }
            let length = usize::try_from(u64::from_be_bytes(header))
                .map_err(|_| HelperError::error("request length does not fit memory"))?;
            if length > MAX_REQUEST_BYTES {
                return Err(HelperError::error("request exceeds the protocol bound"));
            }
            let mut encoded = vec![0_u8; length];
            input
                .read_exact(&mut encoded)
                .map_err(|error| HelperError::error(format!("could not read request: {error}")))?;
            let request: Value = serde_json::from_slice(&encoded)
                .map_err(|error| HelperError::error(format!("invalid request JSON: {error}")))?;
            let request_id = request
                .get("request_id")
                .and_then(Value::as_u64)
                .ok_or_else(|| HelperError::error("request omitted request_id"))?;
            if request.get("protocol").and_then(Value::as_str) != Some(PROTOCOL) {
                return Err(HelperError::error("request used an unsupported protocol"));
            }
            let command = request
                .get("command")
                .ok_or_else(|| HelperError::error("request omitted command"))?;
            let result = match command.get("kind").and_then(Value::as_str) {
                Some("initialize") => validate_initialize_request(command)
                    .and_then(|()| runtime.block_on(portal.initialize())),
                Some("execute") => runtime.block_on(portal.execute(command)),
                Some("emergency_stop") => runtime.block_on(portal.emergency_stop()),
                _ => Err(HelperError::error("request contains an unknown command")),
            };
            let response = match result {
                Ok(result) => json!({
                    "protocol": PROTOCOL,
                    "request_id": request_id,
                    "status": "ok",
                    "output": result,
                }),
                Err(error) => json!({
                    "protocol": PROTOCOL,
                    "request_id": request_id,
                    "status": match error.kind {
                        FailureKind::Denied => "denied",
                        FailureKind::Unavailable => "unavailable",
                        FailureKind::Error => "error",
                    },
                    "message": error.message,
                }),
            };
            let encoded = serde_json::to_vec(&response)
                .map_err(|error| HelperError::error(format!("encode response: {error}")))?;
            output
                .write_all(&(encoded.len() as u64).to_be_bytes())
                .and_then(|()| output.write_all(&encoded))
                .and_then(|()| output.flush())
                .map_err(|error| HelperError::error(format!("write response: {error}")))?;
        }
    }

    fn validate_initialize_request(command: &Value) -> Result<(), HelperError> {
        if command.get("screen_cast").and_then(Value::as_bool) != Some(true)
            || command.get("remote_desktop").and_then(Value::as_bool) != Some(true)
            || command.get("input_transport").and_then(Value::as_str)
                != Some("remote_desktop_notify")
        {
            return Err(HelperError::error(
                "initialize must request ScreenCast and RemoteDesktop notification input",
            ));
        }
        Ok(())
    }

    fn bus_error(kind: FailureKind, context: &str, error: impl std::fmt::Display) -> HelperError {
        HelperError {
            kind,
            message: format!("could not {context}: {error}"),
        }
    }

    fn empty_options() -> HashMap<&'static str, DbusValue<'static>> {
        HashMap::new()
    }

    fn result_u32(
        results: &HashMap<String, OwnedValue>,
        key: &str,
        missing: &str,
    ) -> Result<u32, HelperError> {
        results
            .get(key)
            .ok_or_else(|| HelperError::error(missing))
            .and_then(|value| {
                u32::try_from(value).map_err(|_| HelperError::error("portal value is not uint32"))
            })
    }

    fn result_string(
        results: &HashMap<String, OwnedValue>,
        key: &str,
        missing: &str,
    ) -> Result<String, HelperError> {
        let value = results
            .get(key)
            .ok_or_else(|| HelperError::error(missing))?;
        if let Ok(path) = <&ObjectPath<'_>>::try_from(value) {
            return Ok(path.as_str().to_owned());
        }
        <&str>::try_from(value)
            .map(str::to_owned)
            .map_err(|_| HelperError::error("portal value is not a string/object path"))
    }

    fn object_path(value: &str) -> Result<OwnedObjectPath, HelperError> {
        OwnedObjectPath::try_from(value)
            .map_err(|_| HelperError::error("portal value is not a valid object path"))
    }

    fn dbus_value_has_children(value: &OwnedValue) -> bool {
        match &**value {
            DbusValue::Array(value) => !value.inner().is_empty(),
            DbusValue::Dict(value) => value.iter().next().is_some(),
            DbusValue::Structure(value) => !value.fields().is_empty(),
            DbusValue::Value(_) => true,
            _ => false,
        }
    }

    fn positive_dimensions(value: &OwnedValue) -> Result<(u32, u32), HelperError> {
        let fields = <&Structure<'_>>::try_from(value)
            .map_err(|_| HelperError::error("portal response is missing a tuple child"))?
            .fields();
        let width = fields
            .first()
            .ok_or_else(|| HelperError::error("portal response is missing a tuple child"))
            .and_then(positive_dimension)?;
        let height = fields
            .get(1)
            .ok_or_else(|| HelperError::error("portal response is missing a tuple child"))
            .and_then(positive_dimension)?;
        Ok((width, height))
    }

    fn positive_dimension(value: &DbusValue<'_>) -> Result<u32, HelperError> {
        let value = match u32::try_from(value) {
            Ok(value) => value,
            Err(_) => {
                let signed = i32::try_from(value)
                    .map_err(|_| HelperError::error("portal dimension has an invalid type"))?;
                u32::try_from(signed)
                    .map_err(|_| HelperError::error("portal dimension is negative"))?
            }
        };
        if value == 0 {
            return Err(HelperError::error("portal dimension is zero"));
        }
        Ok(value)
    }

    fn map_screen_point(point: ScreenPoint, viewport: &Value) -> Result<(f64, f64), HelperError> {
        map_point(point.x, point.y, viewport)
    }

    fn map_point(x: u32, y: u32, viewport: &Value) -> Result<(f64, f64), HelperError> {
        let read = |field: &str| {
            viewport
                .get(field)
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .filter(|value| *value > 0)
                .ok_or_else(|| HelperError::error(format!("viewport omitted valid {field}")))
        };
        let display_width = read("display_width")?;
        let display_height = read("display_height")?;
        let image_width = read("image_width")?;
        let image_height = read("image_height")?;
        if x >= image_width || y >= image_height {
            return Err(HelperError::error(
                "input coordinate is outside the delivered screenshot",
            ));
        }
        Ok((
            f64::from(x) * f64::from(display_width) / f64::from(image_width),
            f64::from(y) * f64::from(display_height) / f64::from(image_height),
        ))
    }

    fn keysym_for_char(scalar: char) -> u32 {
        match scalar {
            '\n' | '\r' => 0xff0d,
            '\t' => 0xff09,
            '\u{8}' => 0xff08,
            scalar if scalar as u32 <= 0xff => scalar as u32,
            scalar => 0x0100_0000 | scalar as u32,
        }
    }

    fn keysym_for_key_name(key: &str) -> Option<u32> {
        if key.chars().count() == 1 {
            return key.chars().next().map(keysym_for_char);
        }
        Some(match key.to_ascii_lowercase().as_str() {
            "backspace" => 0xff08,
            "tab" => 0xff09,
            "return" | "enter" => 0xff0d,
            "escape" | "esc" => 0xff1b,
            "home" => 0xff50,
            "left" => 0xff51,
            "up" => 0xff52,
            "right" => 0xff53,
            "down" => 0xff54,
            "pageup" | "page_up" => 0xff55,
            "pagedown" | "page_down" => 0xff56,
            "end" => 0xff57,
            "delete" => 0xffff,
            "space" => u32::from(b' '),
            "plus" => u32::from(b'+'),
            "minus" => u32::from(b'-'),
            "f1" => 0xffbe,
            "f2" => 0xffbf,
            "f3" => 0xffc0,
            "f4" => 0xffc1,
            "f5" => 0xffc2,
            "f6" => 0xffc3,
            "f7" => 0xffc4,
            "f8" => 0xffc5,
            "f9" => 0xffc6,
            "f10" => 0xffc7,
            "f11" => 0xffc8,
            "f12" => 0xffc9,
            _ => return None,
        })
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
}
