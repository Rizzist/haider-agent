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
#[allow(unsafe_code)]
mod linux {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use haider_protocol::computer::{ComputerAction, ScreenPoint, ScrollDirection};
    use haider_protocol::tool::TOOL_RESULT_IMAGE_MAX_SOURCE_BYTES;
    use rustix::io::{FdFlags, fcntl_setfd};
    use serde_json::{Value, json};
    use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_void};
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::process::{Command, Stdio};
    use std::ptr;
    use std::time::{Duration, Instant};

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
    const METHOD_TIMEOUT_MS: i32 = 5_000;
    const CONSENT_TIMEOUT: Duration = Duration::from_secs(55);
    const CAPTURE_TIMEOUT: Duration = Duration::from_secs(12);

    type GBoolean = c_int;
    type GBusType = c_int;
    type GDBusCallFlags = c_int;
    type GDBusSignalFlags = c_int;

    #[repr(C)]
    struct GError {
        domain: c_uint,
        code: c_int,
        message: *mut c_char,
    }

    #[repr(C)]
    struct GDBusConnection {
        _private: [u8; 0],
    }
    #[repr(C)]
    struct GCancellable {
        _private: [u8; 0],
    }
    #[repr(C)]
    struct GVariant {
        _private: [u8; 0],
    }
    #[repr(C)]
    struct GVariantType {
        _private: [u8; 0],
    }
    #[repr(C)]
    struct GUnixFDList {
        _private: [u8; 0],
    }

    type SignalCallback = unsafe extern "C" fn(
        *mut GDBusConnection,
        *const c_char,
        *const c_char,
        *const c_char,
        *const c_char,
        *mut GVariant,
        *mut c_void,
    );

    #[link(name = "gio-2.0")]
    unsafe extern "C" {
        fn g_bus_get_sync(
            bus_type: GBusType,
            cancellable: *mut GCancellable,
            error: *mut *mut GError,
        ) -> *mut GDBusConnection;
        fn g_dbus_connection_get_unique_name(connection: *mut GDBusConnection) -> *const c_char;
        fn g_dbus_connection_signal_subscribe(
            connection: *mut GDBusConnection,
            sender: *const c_char,
            interface_name: *const c_char,
            member: *const c_char,
            object_path: *const c_char,
            arg0: *const c_char,
            flags: GDBusSignalFlags,
            callback: Option<SignalCallback>,
            user_data: *mut c_void,
            user_data_free_func: Option<unsafe extern "C" fn(*mut c_void)>,
        ) -> c_uint;
        fn g_dbus_connection_signal_unsubscribe(
            connection: *mut GDBusConnection,
            subscription_id: c_uint,
        );
        fn g_dbus_connection_call_sync(
            connection: *mut GDBusConnection,
            bus_name: *const c_char,
            object_path: *const c_char,
            interface_name: *const c_char,
            method_name: *const c_char,
            parameters: *mut GVariant,
            reply_type: *const GVariantType,
            flags: GDBusCallFlags,
            timeout_msec: c_int,
            cancellable: *mut GCancellable,
            error: *mut *mut GError,
        ) -> *mut GVariant;
        fn g_dbus_connection_call_with_unix_fd_list_sync(
            connection: *mut GDBusConnection,
            bus_name: *const c_char,
            object_path: *const c_char,
            interface_name: *const c_char,
            method_name: *const c_char,
            parameters: *mut GVariant,
            reply_type: *const GVariantType,
            flags: GDBusCallFlags,
            timeout_msec: c_int,
            fd_list: *mut GUnixFDList,
            out_fd_list: *mut *mut GUnixFDList,
            cancellable: *mut GCancellable,
            error: *mut *mut GError,
        ) -> *mut GVariant;
        fn g_unix_fd_list_get(
            list: *mut GUnixFDList,
            index: c_int,
            error: *mut *mut GError,
        ) -> c_int;
    }

    #[link(name = "gobject-2.0")]
    unsafe extern "C" {
        fn g_object_unref(object: *mut c_void);
    }

    #[link(name = "glib-2.0")]
    unsafe extern "C" {
        fn g_error_free(error: *mut GError);
        fn g_main_context_iteration(context: *mut c_void, may_block: GBoolean) -> GBoolean;
        fn g_variant_get_child_value(value: *mut GVariant, index: usize) -> *mut GVariant;
        fn g_variant_get_handle(value: *mut GVariant) -> c_int;
        fn g_variant_get_int32(value: *mut GVariant) -> i32;
        fn g_variant_get_string(value: *mut GVariant, length: *mut usize) -> *const c_char;
        fn g_variant_get_type_string(value: *mut GVariant) -> *const c_char;
        fn g_variant_get_uint32(value: *mut GVariant) -> u32;
        fn g_variant_lookup_value(
            dictionary: *mut GVariant,
            key: *const c_char,
            expected_type: *const GVariantType,
        ) -> *mut GVariant;
        fn g_variant_n_children(value: *mut GVariant) -> usize;
        fn g_variant_parse(
            type_: *const GVariantType,
            text: *const c_char,
            limit: *const c_char,
            endptr: *mut *const c_char,
            error: *mut *mut GError,
        ) -> *mut GVariant;
        fn g_variant_ref(value: *mut GVariant) -> *mut GVariant;
        fn g_variant_ref_sink(value: *mut GVariant) -> *mut GVariant;
        fn g_variant_type_free(type_: *mut GVariantType);
        fn g_variant_type_new(type_string: *const c_char) -> *mut GVariantType;
        fn g_variant_unref(value: *mut GVariant);
    }

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

    struct Variant(*mut GVariant);

    impl Variant {
        fn child(&self, index: usize) -> Result<Self, HelperError> {
            if index >= self.children() {
                return Err(HelperError::error(
                    "portal response is missing a tuple child",
                ));
            }
            // SAFETY: the index is bounded by g_variant_n_children.
            let child = unsafe { g_variant_get_child_value(self.0, index) };
            if child.is_null() {
                Err(HelperError::error("portal returned a null variant child"))
            } else {
                Ok(Self(child))
            }
        }

        fn children(&self) -> usize {
            // SAFETY: self owns a live GVariant.
            unsafe { g_variant_n_children(self.0) }
        }

        fn lookup(&self, key: &str) -> Result<Option<Self>, HelperError> {
            let key = cstring(key)?;
            // SAFETY: dictionary and key remain live for this call.
            let value = unsafe { g_variant_lookup_value(self.0, key.as_ptr(), ptr::null()) };
            Ok((!value.is_null()).then_some(Self(value)))
        }

        fn type_string(&self) -> Result<&str, HelperError> {
            // SAFETY: GLib returns an interned string for a live variant.
            let value = unsafe { g_variant_get_type_string(self.0) };
            if value.is_null() {
                return Err(HelperError::error("portal variant omitted its type"));
            }
            // SAFETY: GLib type strings are NUL-terminated UTF-8 ASCII.
            unsafe { CStr::from_ptr(value) }
                .to_str()
                .map_err(|_| HelperError::error("portal variant type is not UTF-8"))
        }

        fn string(&self) -> Result<String, HelperError> {
            if !matches!(self.type_string()?, "s" | "o") {
                return Err(HelperError::error(
                    "portal value is not a string/object path",
                ));
            }
            let mut length = 0;
            // SAFETY: the value has a string-compatible type.
            let value = unsafe { g_variant_get_string(self.0, &mut length) };
            if value.is_null() {
                return Err(HelperError::error("portal returned a null string"));
            }
            // SAFETY: GLib owns a live buffer of exactly length bytes.
            let bytes = unsafe { std::slice::from_raw_parts(value.cast::<u8>(), length) };
            String::from_utf8(bytes.to_vec())
                .map_err(|_| HelperError::error("portal returned a non-UTF-8 string"))
        }

        fn u32(&self) -> Result<u32, HelperError> {
            if self.type_string()? != "u" {
                return Err(HelperError::error("portal value is not uint32"));
            }
            // SAFETY: the variant type was checked.
            Ok(unsafe { g_variant_get_uint32(self.0) })
        }

        fn handle(&self) -> Result<c_int, HelperError> {
            if self.type_string()? != "h" {
                return Err(HelperError::error(
                    "portal value is not a file descriptor handle",
                ));
            }
            // SAFETY: the variant type was checked.
            Ok(unsafe { g_variant_get_handle(self.0) })
        }

        fn positive_dimension(&self) -> Result<u32, HelperError> {
            let value = match self.type_string()? {
                "u" => self.u32()?,
                "i" => {
                    // SAFETY: the variant type was checked.
                    u32::try_from(unsafe { g_variant_get_int32(self.0) })
                        .map_err(|_| HelperError::error("portal dimension is negative"))?
                }
                _ => return Err(HelperError::error("portal dimension has an invalid type")),
            };
            if value == 0 {
                Err(HelperError::error("portal dimension is zero"))
            } else {
                Ok(value)
            }
        }
    }

    impl Drop for Variant {
        fn drop(&mut self) {
            // SAFETY: Variant owns one non-null reference.
            unsafe { g_variant_unref(self.0) };
        }
    }

    struct Portal {
        connection: *mut GDBusConnection,
        sender_component: String,
        session_handle: Option<String>,
        stream_node: Option<u32>,
        logical_size: Option<(u32, u32)>,
        next_token: u64,
        left_button_down: bool,
    }

    impl Portal {
        fn connect() -> Result<Self, HelperError> {
            let mut error = ptr::null_mut();
            // SAFETY: null cancellable is supported; error is a writable out pointer.
            let connection = unsafe { g_bus_get_sync(2, ptr::null_mut(), &mut error) };
            if connection.is_null() {
                return Err(glib_error(
                    FailureKind::Unavailable,
                    error,
                    "connect to the D-Bus session bus",
                ));
            }
            // SAFETY: connection is live.
            let unique = unsafe { g_dbus_connection_get_unique_name(connection) };
            if unique.is_null() {
                // SAFETY: connection owns one GObject reference.
                unsafe { g_object_unref(connection.cast()) };
                return Err(HelperError::unavailable(
                    "D-Bus session connection has no unique name",
                ));
            }
            // SAFETY: unique name is a live NUL-terminated D-Bus string.
            let unique = unsafe { CStr::from_ptr(unique) }
                .to_str()
                .map_err(|_| HelperError::error("D-Bus unique name is not UTF-8"))?;
            let sender_component = unique
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

        fn initialize(&mut self) -> Result<Value, HelperError> {
            let create_token = self.token("create");
            let session_token = self.token("session");
            let create = self.portal_request(
                REMOTE_DESKTOP,
                "CreateSession",
                "(a{sv})",
                &format!(
                    "({{'handle_token': <'{create_token}'>, 'session_handle_token': <'{session_token}'>}},)"
                ),
                &create_token,
            )?;
            let session_handle = create
                .lookup("session_handle")?
                .ok_or_else(|| HelperError::error("CreateSession omitted session_handle"))?
                .string()?;
            self.session_handle = Some(session_handle.clone());

            let devices_token = self.token("devices");
            self.portal_request(
                REMOTE_DESKTOP,
                "SelectDevices",
                "(oa{sv})",
                &format!(
                    "('{session_handle}', {{'handle_token': <'{devices_token}'>, 'types': <uint32 {}>}})",
                    DEVICE_POINTER | DEVICE_KEYBOARD
                ),
                &devices_token,
            )?;

            let sources_token = self.token("sources");
            self.portal_request(
                SCREEN_CAST,
                "SelectSources",
                "(oa{sv})",
                &format!(
                    "('{session_handle}', {{'handle_token': <'{sources_token}'>, 'types': <uint32 {SOURCE_MONITOR}>, 'multiple': <false>, 'cursor_mode': <uint32 {CURSOR_EMBEDDED}>}})"
                ),
                &sources_token,
            )?;

            let start_token = self.token("start");
            let started = self.portal_request(
                REMOTE_DESKTOP,
                "Start",
                "(osa{sv})",
                &format!("('{session_handle}', '', {{'handle_token': <'{start_token}'>}})"),
                &start_token,
            )?;
            let devices = started
                .lookup("devices")?
                .ok_or_else(|| HelperError::error("RemoteDesktop.Start omitted devices"))?
                .u32()?;
            if devices & (DEVICE_POINTER | DEVICE_KEYBOARD) != DEVICE_POINTER | DEVICE_KEYBOARD {
                return Err(HelperError {
                    kind: FailureKind::Denied,
                    message:
                        "interactive portal consent did not grant pointer and keyboard control"
                            .into(),
                });
            }
            let streams = started.lookup("streams")?.ok_or_else(|| {
                HelperError::error("RemoteDesktop.Start omitted ScreenCast streams")
            })?;
            let stream = streams.child(0).map_err(|_| HelperError {
                kind: FailureKind::Denied,
                message: "interactive portal consent did not grant a monitor stream".into(),
            })?;
            let node = stream.child(0)?.u32()?;
            let properties = stream.child(1)?;
            let size = properties
                .lookup("logical_size")?
                .or(properties.lookup("size")?)
                .ok_or_else(|| HelperError::error("ScreenCast stream omitted logical size"))?;
            let logical_width = size.child(0)?.positive_dimension()?;
            let logical_height = size.child(1)?.positive_dimension()?;
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

        fn execute(&mut self, command: &Value) -> Result<Value, HelperError> {
            let action: ComputerAction = serde_json::from_value(
                command
                    .get("action")
                    .cloned()
                    .ok_or_else(|| HelperError::error("execute omitted action"))?,
            )
            .map_err(|error| HelperError::error(format!("invalid computer action: {error}")))?;
            if matches!(action, ComputerAction::Screenshot) {
                return self.screenshot();
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
            self.input(&action, viewport)?;
            Ok(json!({"action": action_name(&action)}))
        }

        fn screenshot(&mut self) -> Result<Value, HelperError> {
            let node = self
                .stream_node
                .ok_or_else(|| HelperError::error("ScreenCast stream is not initialized"))?;
            let (logical_width, logical_height) = self
                .logical_size
                .ok_or_else(|| HelperError::error("ScreenCast logical size is unavailable"))?;
            let remote = self.open_pipewire_remote()?;
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

        fn input(&mut self, action: &ComputerAction, viewport: &Value) -> Result<(), HelperError> {
            match action {
                ComputerAction::LeftClick { x, y } => {
                    self.pointer_absolute(map_point(*x, *y, viewport)?)?;
                    self.button(BUTTON_LEFT, true)?;
                    self.button(BUTTON_LEFT, false)
                }
                ComputerAction::RightClick => {
                    self.button(BUTTON_RIGHT, true)?;
                    self.button(BUTTON_RIGHT, false)
                }
                ComputerAction::MiddleClick => {
                    self.button(BUTTON_MIDDLE, true)?;
                    self.button(BUTTON_MIDDLE, false)
                }
                ComputerAction::DoubleClick => {
                    for _ in 0..2 {
                        self.button(BUTTON_LEFT, true)?;
                        self.button(BUTTON_LEFT, false)?;
                    }
                    Ok(())
                }
                ComputerAction::LeftMouseDown => {
                    if self.left_button_down {
                        return Err(HelperError::error("left mouse button is already held"));
                    }
                    self.button(BUTTON_LEFT, true)?;
                    self.left_button_down = true;
                    Ok(())
                }
                ComputerAction::LeftMouseUp => {
                    if !self.left_button_down {
                        return Err(HelperError::error("left mouse button is not held"));
                    }
                    self.button(BUTTON_LEFT, false)?;
                    self.left_button_down = false;
                    Ok(())
                }
                ComputerAction::MouseMove { x, y } => {
                    self.pointer_absolute(map_point(*x, *y, viewport)?)
                }
                ComputerAction::LeftClickDrag { from, to } => {
                    self.pointer_absolute(map_screen_point(*from, viewport)?)?;
                    self.button(BUTTON_LEFT, true)?;
                    self.pointer_absolute(map_screen_point(*to, viewport)?)?;
                    self.button(BUTTON_LEFT, false)
                }
                ComputerAction::Type { text } => {
                    for scalar in text.chars() {
                        self.keysym(keysym_for_char(scalar), true)?;
                        self.keysym(keysym_for_char(scalar), false)?;
                    }
                    Ok(())
                }
                ComputerAction::Key { keys } => self.key_chord(keys),
                ComputerAction::Scroll {
                    x,
                    y,
                    direction,
                    amount,
                } => {
                    self.pointer_absolute(map_point(*x, *y, viewport)?)?;
                    let steps = i32::try_from(*amount).map_err(|_| {
                        HelperError::error("scroll amount exceeds the portal int32 step range")
                    })?;
                    let (axis, steps) = match direction {
                        ScrollDirection::Up => (0, -steps),
                        ScrollDirection::Down => (0, steps),
                        ScrollDirection::Left => (1, -steps),
                        ScrollDirection::Right => (1, steps),
                    };
                    self.pointer_axis_discrete(axis, steps)
                }
                ComputerAction::Wait { .. } => Ok(()),
                ComputerAction::Screenshot
                | ComputerAction::CursorPosition
                | ComputerAction::Inspect { .. } => unreachable!(),
            }
        }

        fn pointer_absolute(&self, point: (f64, f64)) -> Result<(), HelperError> {
            let session = self.session()?;
            let stream = self
                .stream_node
                .ok_or_else(|| HelperError::error("no stream"))?;
            self.call_empty(
                REMOTE_DESKTOP,
                "NotifyPointerMotionAbsolute",
                "(oa{sv}udd)",
                &format!(
                    "('{session}', {{}}, uint32 {stream}, {}, {})",
                    point.0, point.1
                ),
            )
        }

        fn button(&self, button: i32, pressed: bool) -> Result<(), HelperError> {
            let session = self.session()?;
            let state = if pressed {
                STATE_PRESSED
            } else {
                STATE_RELEASED
            };
            self.call_empty(
                REMOTE_DESKTOP,
                "NotifyPointerButton",
                "(oa{sv}iu)",
                &format!("('{session}', {{}}, {button}, uint32 {state})"),
            )
        }

        fn pointer_axis_discrete(&self, axis: u32, steps: i32) -> Result<(), HelperError> {
            let session = self.session()?;
            self.call_empty(
                REMOTE_DESKTOP,
                "NotifyPointerAxisDiscrete",
                "(oa{sv}ui)",
                &format!("('{session}', {{}}, uint32 {axis}, {steps})"),
            )
        }

        fn keysym(&self, keysym: u32, pressed: bool) -> Result<(), HelperError> {
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
                "(oa{sv}iu)",
                &format!("('{session}', {{}}, {keysym}, uint32 {state})"),
            )
        }

        fn key_chord(&self, chord: &str) -> Result<(), HelperError> {
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
                self.keysym(*modifier, true)?;
            }
            let result = self
                .keysym(main, true)
                .and_then(|()| self.keysym(main, false));
            for modifier in modifiers.iter().rev() {
                self.keysym(*modifier, false)?;
            }
            result
        }

        fn emergency_stop(&mut self) -> Result<Value, HelperError> {
            if self.left_button_down {
                self.button(BUTTON_LEFT, false)?;
                self.left_button_down = false;
            }
            self.close_session();
            Ok(json!({"stopped": true}))
        }

        fn open_pipewire_remote(&self) -> Result<OwnedFd, HelperError> {
            let session = self.session()?;
            let parameters = variant("(oa{sv})", &format!("('{session}', {{}})"))?;
            let mut error = ptr::null_mut();
            let mut fd_list = ptr::null_mut();
            // SAFETY: all pointers are live for this synchronous call.
            let reply = unsafe {
                g_dbus_connection_call_with_unix_fd_list_sync(
                    self.connection,
                    cstring(PORTAL_DESTINATION)?.as_ptr(),
                    cstring(PORTAL_PATH)?.as_ptr(),
                    cstring(SCREEN_CAST)?.as_ptr(),
                    cstring("OpenPipeWireRemote")?.as_ptr(),
                    parameters.0,
                    ptr::null(),
                    0,
                    METHOD_TIMEOUT_MS,
                    ptr::null_mut(),
                    &mut fd_list,
                    ptr::null_mut(),
                    &mut error,
                )
            };
            if reply.is_null() || fd_list.is_null() {
                if !reply.is_null() {
                    // SAFETY: reply owns one variant reference.
                    unsafe { g_variant_unref(reply) };
                }
                if !fd_list.is_null() {
                    // SAFETY: fd_list owns one GObject reference.
                    unsafe { g_object_unref(fd_list.cast()) };
                }
                return Err(glib_error(
                    FailureKind::Unavailable,
                    error,
                    "open the ScreenCast PipeWire remote",
                ));
            }
            let reply = Variant(reply);
            let index = reply.child(0)?.handle()?;
            // SAFETY: fd_list is live and index came from the matching reply.
            let fd = unsafe { g_unix_fd_list_get(fd_list, index, &mut error) };
            // SAFETY: fd_list owns one GObject reference.
            unsafe { g_object_unref(fd_list.cast()) };
            if fd < 0 {
                return Err(glib_error(
                    FailureKind::Unavailable,
                    error,
                    "receive the ScreenCast PipeWire descriptor",
                ));
            }
            // SAFETY: g_unix_fd_list_get returns a newly duplicated owned fd.
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        }

        fn portal_request(
            &mut self,
            interface: &str,
            method: &str,
            signature: &str,
            text: &str,
            token: &str,
        ) -> Result<Variant, HelperError> {
            let request_path = format!(
                "/org/freedesktop/portal/desktop/request/{}/{}",
                self.sender_component, token
            );
            let mut capture = SignalCapture {
                parameters: ptr::null_mut(),
            };
            // SAFETY: strings and capture outlive the subscription; callback runs on this thread.
            let subscription = unsafe {
                g_dbus_connection_signal_subscribe(
                    self.connection,
                    cstring(PORTAL_DESTINATION)?.as_ptr(),
                    cstring(REQUEST)?.as_ptr(),
                    cstring("Response")?.as_ptr(),
                    cstring(&request_path)?.as_ptr(),
                    ptr::null(),
                    0,
                    Some(capture_signal),
                    (&raw mut capture).cast(),
                    None,
                )
            };
            if subscription == 0 {
                return Err(HelperError::unavailable(
                    "could not subscribe to the portal response signal",
                ));
            }
            let subscription = PortalRequestSubscription {
                connection: self.connection,
                id: subscription,
            };
            let reply = self.call(PORTAL_PATH, interface, method, signature, text)?;
            let returned_path = reply.child(0)?.string()?;
            if returned_path != request_path {
                return Err(HelperError::error(
                    "portal returned a request path that did not match handle_token",
                ));
            }
            let deadline = Instant::now() + CONSENT_TIMEOUT;
            while capture.parameters.is_null() && Instant::now() < deadline {
                // SAFETY: iterating the default context dispatches the subscribed signal callback.
                unsafe { g_main_context_iteration(ptr::null_mut(), 0) };
                std::thread::sleep(Duration::from_millis(10));
            }
            // Stop callbacks before inspecting or dropping their stack-backed capture target.
            drop(subscription);
            if capture.parameters.is_null() {
                return Err(HelperError::unavailable(
                    "portal interaction timed out waiting for interactive consent",
                ));
            }
            let parameters = Variant(std::mem::replace(&mut capture.parameters, ptr::null_mut()));
            let response = parameters.child(0)?.u32()?;
            match response {
                0 => parameters.child(1),
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

        fn call(
            &self,
            path: &str,
            interface: &str,
            method: &str,
            signature: &str,
            text: &str,
        ) -> Result<Variant, HelperError> {
            let parameters = variant(signature, text)?;
            let mut error = ptr::null_mut();
            // SAFETY: all pointers are live for this synchronous call.
            let reply = unsafe {
                g_dbus_connection_call_sync(
                    self.connection,
                    cstring(PORTAL_DESTINATION)?.as_ptr(),
                    cstring(path)?.as_ptr(),
                    cstring(interface)?.as_ptr(),
                    cstring(method)?.as_ptr(),
                    parameters.0,
                    ptr::null(),
                    0,
                    METHOD_TIMEOUT_MS,
                    ptr::null_mut(),
                    &mut error,
                )
            };
            if reply.is_null() {
                Err(glib_error(
                    FailureKind::Unavailable,
                    error,
                    &format!("call portal method {interface}.{method}"),
                ))
            } else {
                Ok(Variant(reply))
            }
        }

        fn call_empty(
            &self,
            interface: &str,
            method: &str,
            signature: &str,
            text: &str,
        ) -> Result<(), HelperError> {
            drop(self.call(PORTAL_PATH, interface, method, signature, text)?);
            Ok(())
        }

        fn close_session(&mut self) {
            let Some(session) = self.session_handle.take() else {
                return;
            };
            let _ = self.call(&session, SESSION, "Close", "()", "()");
            self.stream_node = None;
            self.logical_size = None;
        }

        fn session(&self) -> Result<&str, HelperError> {
            self.session_handle
                .as_deref()
                .ok_or_else(|| HelperError::error("portal session is not initialized"))
        }

        fn token(&mut self, prefix: &str) -> String {
            let token = format!("haider_{prefix}_{}_{}", std::process::id(), self.next_token);
            self.next_token = self.next_token.wrapping_add(1);
            token
        }
    }

    impl Drop for Portal {
        fn drop(&mut self) {
            self.close_session();
            // SAFETY: connection owns one GObject reference.
            unsafe { g_object_unref(self.connection.cast()) };
        }
    }

    struct SignalCapture {
        parameters: *mut GVariant,
    }

    impl Drop for SignalCapture {
        fn drop(&mut self) {
            if !self.parameters.is_null() {
                // SAFETY: capture_signal retained exactly one reference for this field.
                unsafe { g_variant_unref(self.parameters) };
            }
        }
    }

    struct PortalRequestSubscription {
        connection: *mut GDBusConnection,
        id: c_uint,
    }

    impl Drop for PortalRequestSubscription {
        fn drop(&mut self) {
            // SAFETY: this id came from this live connection and is dropped exactly once.
            unsafe { g_dbus_connection_signal_unsubscribe(self.connection, self.id) };
        }
    }

    unsafe extern "C" fn capture_signal(
        _connection: *mut GDBusConnection,
        _sender_name: *const c_char,
        _object_path: *const c_char,
        _interface_name: *const c_char,
        _signal_name: *const c_char,
        parameters: *mut GVariant,
        user_data: *mut c_void,
    ) {
        if parameters.is_null() || user_data.is_null() {
            return;
        }
        // SAFETY: user_data points to the live SignalCapture installed by portal_request.
        let capture = unsafe { &mut *user_data.cast::<SignalCapture>() };
        if capture.parameters.is_null() {
            // SAFETY: the signal supplies a live borrowed variant; retain one reference.
            capture.parameters = unsafe { g_variant_ref(parameters) };
        }
    }

    pub(super) fn run() -> Result<(), HelperError> {
        let args = std::env::args().skip(1).collect::<Vec<_>>();
        if args.as_slice() != ["serve", "--protocol", PROTOCOL] {
            return Err(HelperError::error(
                "usage: haider-wayland-portal serve --protocol haider-cu-wayland-v1",
            ));
        }
        let mut portal = Portal::connect()?;
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
                Some("initialize") => {
                    validate_initialize_request(command).and_then(|()| portal.initialize())
                }
                Some("execute") => portal.execute(command),
                Some("emergency_stop") => portal.emergency_stop(),
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

    fn variant(signature: &str, text: &str) -> Result<Variant, HelperError> {
        let signature = cstring(signature)?;
        // SAFETY: signature is a NUL-terminated GVariant signature.
        let type_ = unsafe { g_variant_type_new(signature.as_ptr()) };
        if type_.is_null() {
            return Err(HelperError::error("invalid GVariant signature"));
        }
        let text = cstring(text)?;
        let mut error = ptr::null_mut();
        // SAFETY: type and text remain live for the parse call.
        let value = unsafe {
            g_variant_parse(
                type_,
                text.as_ptr(),
                ptr::null(),
                ptr::null_mut(),
                &mut error,
            )
        };
        // SAFETY: type_ owns one allocated type descriptor.
        unsafe { g_variant_type_free(type_) };
        if value.is_null() {
            return Err(glib_error(
                FailureKind::Error,
                error,
                "parse portal GVariant parameters",
            ));
        }
        // SAFETY: value is live; sinking normalizes floating ownership.
        Ok(Variant(unsafe { g_variant_ref_sink(value) }))
    }

    fn glib_error(kind: FailureKind, error: *mut GError, context: &str) -> HelperError {
        if error.is_null() {
            return HelperError {
                kind,
                message: format!("could not {context}: no GLib error detail"),
            };
        }
        // SAFETY: GLib returned a live GError and NUL-terminated message.
        let message = unsafe {
            if (*error).message.is_null() {
                "no GLib error detail".into()
            } else {
                CStr::from_ptr((*error).message)
                    .to_string_lossy()
                    .into_owned()
            }
        };
        // SAFETY: error owns one GLib allocation.
        unsafe { g_error_free(error) };
        HelperError {
            kind,
            message: format!("could not {context}: {message}"),
        }
    }

    fn cstring(value: &str) -> Result<CString, HelperError> {
        CString::new(value)
            .map_err(|_| HelperError::error("portal string contains an interior NUL"))
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
