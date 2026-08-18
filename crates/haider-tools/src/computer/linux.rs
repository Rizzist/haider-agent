//! Linux X11 backend.
//!
//! The pure-Rust `x11rb` connection captures the root drawable with
//! `GetImage` and injects input through XTEST. CU-1 still owns image admission
//! and downscaling: [`ComputerBackend::set_viewport`] records the dimensions
//! actually delivered to the model, and all later model coordinates are
//! scaled back into root-window pixels as
//! `floor(model_pixel * root_extent / admitted_extent)` (cursor reporting
//! applies the inverse mapping).

use super::{ComputerBackend, ComputerCancelToken, ComputerError, ComputerOutput, ComputerResult};
use async_trait::async_trait;
use haider_protocol::computer::{ComputerAction, ScreenPoint, ScrollDirection};
use image::{DynamicImage, ImageFormat, RgbImage};
use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::time::{Duration, Instant};
use x11rb::CURRENT_TIME;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, ConnectionExt as _, ImageFormat as XImageFormat,
    ImageOrder, KEY_PRESS_EVENT, KEY_RELEASE_EVENT, MOTION_NOTIFY_EVENT, VisualClass,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

const BUTTON_LEFT: u8 = 1;
const BUTTON_MIDDLE: u8 = 2;
const BUTTON_RIGHT: u8 = 3;
const BUTTON_SCROLL_UP: u8 = 4;
const BUTTON_SCROLL_DOWN: u8 = 5;
const BUTTON_SCROLL_LEFT: u8 = 6;
const BUTTON_SCROLL_RIGHT: u8 = 7;
const X11_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

const XK_BACK_SPACE: u32 = 0xff08;
const XK_TAB: u32 = 0xff09;
const XK_RETURN: u32 = 0xff0d;
const XK_ESCAPE: u32 = 0xff1b;
const XK_HOME: u32 = 0xff50;
const XK_LEFT: u32 = 0xff51;
const XK_UP: u32 = 0xff52;
const XK_RIGHT: u32 = 0xff53;
const XK_DOWN: u32 = 0xff54;
const XK_PAGE_UP: u32 = 0xff55;
const XK_PAGE_DOWN: u32 = 0xff56;
const XK_END: u32 = 0xff57;
const XK_DELETE: u32 = 0xffff;
const XK_SHIFT_L: u32 = 0xffe1;
const XK_CONTROL_L: u32 = 0xffe3;
const XK_ALT_L: u32 = 0xffe9;
const XK_SUPER_L: u32 = 0xffeb;

static NEXT_INPUT_OWNER: AtomicU64 = AtomicU64::new(1);
static INPUT_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static HELD_LEFT_OWNER: Mutex<Option<u64>> = Mutex::new(None);

type KeySym = u32;
type KeyCode = u8;

#[derive(Debug, Clone, Copy)]
struct Viewport {
    display_width: u32,
    display_height: u32,
    image_width: u32,
    image_height: u32,
}

#[derive(Debug, Clone, Copy)]
struct NativePoint {
    x: i16,
    y: i16,
}

#[derive(Debug, Clone, Copy)]
struct KeyBinding {
    keycode: KeyCode,
    shift: bool,
}

struct X11Connection {
    connection: RustConnection,
    display_name: String,
    screen_number: usize,
    root: u32,
    xtest_available: bool,
}

impl X11Connection {
    fn connect(display_name: String) -> ComputerResult<Self> {
        let (connection, screen_number) =
            x11rb::connect(Some(&display_name)).map_err(|error| ComputerError::Unavailable {
                platform: "linux".into(),
                message: format!(
                    "could not connect to X11 display `{display_name}`: {error}; verify $DISPLAY and X server access (for CI, start Xvfb)"
                ),
            })?;
        let root = connection
            .setup()
            .roots
            .get(screen_number)
            .ok_or_else(|| ComputerError::Unavailable {
                platform: "linux".into(),
                message: format!(
                    "X11 display `{display_name}` did not provide screen {screen_number}; verify the screen suffix in $DISPLAY"
                ),
            })?
            .root;
        let xtest_available = match connection.xtest_get_version(2, 2) {
            Ok(cookie) => cookie.reply().is_ok(),
            Err(_) => false,
        };
        Ok(Self {
            connection,
            display_name,
            screen_number,
            root,
            xtest_available,
        })
    }

    fn require_xtest(&self) -> ComputerResult<()> {
        if self.xtest_available {
            Ok(())
        } else {
            Err(ComputerError::Unavailable {
                platform: "linux".into(),
                message: format!(
                    "X11 display `{}` does not provide the XTEST extension required for computer control (Xvfb enables XTEST by default)",
                    self.display_name
                ),
            })
        }
    }
}

#[derive(Default)]
struct BackendState {
    connection: Option<X11Connection>,
    pending_display_size: Option<(u32, u32)>,
    viewport: Option<Viewport>,
    left_button_down: bool,
}

struct CaptureFrame {
    width: u32,
    height: u32,
    bits_per_pixel: u8,
    scanline_pad: u8,
    byte_order: ImageOrder,
    red_mask: u32,
    green_mask: u32,
    blue_mask: u32,
    data: Vec<u8>,
}

pub(crate) struct LinuxComputerBackend {
    input_owner: u64,
    state: Arc<Mutex<BackendState>>,
    connect_gate: tokio::sync::Mutex<()>,
}

impl LinuxComputerBackend {
    pub(crate) fn new() -> Self {
        Self {
            input_owner: NEXT_INPUT_OWNER.fetch_add(1, Ordering::Relaxed),
            state: Arc::new(Mutex::new(BackendState::default())),
            connect_gate: tokio::sync::Mutex::new(()),
        }
    }

    fn lock_state(&self) -> ComputerResult<MutexGuard<'_, BackendState>> {
        self.state.lock().map_err(|_| ComputerError::Backend {
            message: "Linux X11 computer backend lock is poisoned".into(),
        })
    }

    fn display_name() -> ComputerResult<String> {
        let display = std::env::var_os("DISPLAY").ok_or_else(|| ComputerError::Unavailable {
            platform: "linux".into(),
            message: "Linux computer use requires an X11 display; set $DISPLAY to a running X server (for CI, start Xvfb)".into(),
        })?;
        if display.is_empty() {
            return Err(ComputerError::Unavailable {
                platform: "linux".into(),
                message: "Linux computer use requires a non-empty $DISPLAY pointing to a running X server (for CI, start Xvfb)".into(),
            });
        }
        display
            .into_string()
            .map_err(|_| ComputerError::Unavailable {
                platform: "linux".into(),
                message: "$DISPLAY is not valid UTF-8; set it to an X11 display such as :0".into(),
            })
    }

    /// X11 allows TCP display names whose OS connect timeout can be minutes.
    /// Keep daemon dispatch bounded without leaving a Tokio blocking worker
    /// behind: a helper thread owns only its candidate connection, and drops it
    /// if the receiver has gone away after timeout or cancellation.
    fn connect_bounded(
        display_name: String,
        cancel: &ComputerCancelToken,
    ) -> ComputerResult<X11Connection> {
        let display_for_error = display_name.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        let _worker = std::thread::Builder::new()
            .name("haider-x11-connect".into())
            .spawn(move || {
                let _ = sender.send(X11Connection::connect(display_name));
            })
            .map_err(|error| ComputerError::Backend {
                message: format!("could not start the Linux X11 connection worker: {error}"),
            })?;
        let deadline = Instant::now() + X11_CONNECT_TIMEOUT;
        loop {
            cancel.check()?;
            let now = Instant::now();
            if now >= deadline {
                return Err(ComputerError::Unavailable {
                    platform: "linux".into(),
                    message: format!(
                        "timed out connecting to X11 display `{display_for_error}` after {} seconds; verify $DISPLAY and X server access (for CI, start Xvfb)",
                        X11_CONNECT_TIMEOUT.as_secs()
                    ),
                });
            }
            let wait = Duration::from_millis(10).min(deadline.saturating_duration_since(now));
            match receiver.recv_timeout(wait) {
                Ok(result) => return result,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(ComputerError::Backend {
                        message: "Linux X11 connection worker stopped without a result".into(),
                    });
                }
            }
        }
    }

    async fn ensure_connection(&self, cancel: &ComputerCancelToken) -> ComputerResult<()> {
        if self.lock_state()?.connection.is_some() {
            return Ok(());
        }
        let _connect_gate = tokio::select! {
            gate = self.connect_gate.lock() => gate,
            () = wait_for_cancel(cancel) => return Err(ComputerError::Cancelled),
        };
        cancel.check()?;
        if self.lock_state()?.connection.is_some() {
            return Ok(());
        }
        let display_name = Self::display_name()?;
        let worker_cancel = cancel.clone();
        let connection = tokio::task::spawn_blocking(move || {
            Self::connect_bounded(display_name, &worker_cancel)
        })
        .await
        .map_err(|error| ComputerError::Backend {
            message: format!("Linux X11 connection worker failed: {error}"),
        })??;
        let mut state = self.lock_state()?;
        cancel.check()?;
        state.connection = Some(connection);
        Ok(())
    }

    fn connection(state: &BackendState) -> ComputerResult<&X11Connection> {
        state
            .connection
            .as_ref()
            .ok_or_else(|| ComputerError::Backend {
                message: "Linux X11 connection was not initialized".into(),
            })
    }

    fn ensure_control_available(&self) -> ComputerResult<()> {
        let state = self.lock_state()?;
        Self::connection(&state)?.require_xtest()
    }

    fn capture_frame(state: &Mutex<BackendState>) -> ComputerResult<CaptureFrame> {
        let state = state.lock().map_err(|_| ComputerError::Backend {
            message: "Linux X11 computer backend lock is poisoned".into(),
        })?;
        let x11 = Self::connection(&state)?;
        let geometry = x11
            .connection
            .get_geometry(x11.root)
            .map_err(|error| x11_backend("could not request X11 root geometry", error))?
            .reply()
            .map_err(|error| x11_backend("could not read X11 root geometry", error))?;
        if geometry.width == 0 || geometry.height == 0 {
            return Err(ComputerError::Backend {
                message: "X11 root window reported empty dimensions".into(),
            });
        }
        let reply = x11
            .connection
            .get_image(
                XImageFormat::Z_PIXMAP,
                x11.root,
                0,
                0,
                geometry.width,
                geometry.height,
                u32::MAX,
            )
            .map_err(|error| x11_backend("could not request an X11 root screenshot", error))?
            .reply()
            .map_err(|error| {
                x11_backend(
                    &format!(
                        "could not capture the root window on X11 display `{}`",
                        x11.display_name
                    ),
                    error,
                )
            })?;
        let format = x11
            .connection
            .setup()
            .pixmap_formats
            .iter()
            .find(|format| format.depth == reply.depth)
            .ok_or_else(|| ComputerError::Backend {
                message: format!(
                    "X11 did not describe the {}-bit root pixmap format",
                    reply.depth
                ),
            })?;
        let screen = x11
            .connection
            .setup()
            .roots
            .get(x11.screen_number)
            .ok_or_else(|| ComputerError::Backend {
                message: "the connected X11 screen disappeared from setup metadata".into(),
            })?;
        let visual = screen
            .allowed_depths
            .iter()
            .flat_map(|depth| depth.visuals.iter())
            .find(|visual| visual.visual_id == reply.visual)
            .ok_or_else(|| ComputerError::Backend {
                message: format!(
                    "X11 screenshot returned unknown root visual 0x{:x}",
                    reply.visual
                ),
            })?;
        if visual.class != VisualClass::TRUE_COLOR {
            return Err(ComputerError::Backend {
                message: "the X11 root visual is not TrueColor; use a TrueColor X server (Xvfb: -screen 0 <width>x<height>x24)".into(),
            });
        }
        Ok(CaptureFrame {
            width: u32::from(geometry.width),
            height: u32::from(geometry.height),
            bits_per_pixel: format.bits_per_pixel,
            scanline_pad: format.scanline_pad,
            byte_order: x11.connection.setup().image_byte_order,
            red_mask: visual.red_mask,
            green_mask: visual.green_mask,
            blue_mask: visual.blue_mask,
            data: reply.data,
        })
    }

    fn capture_png_blocking(
        state: &Mutex<BackendState>,
        cancel: &ComputerCancelToken,
    ) -> ComputerResult<Vec<u8>> {
        cancel.check()?;
        // Hold the connection lock only across the protocol round-trip. Pixel
        // conversion and PNG encoding cannot strand control operations behind
        // a cancelled capture.
        let frame = Self::capture_frame(state)?;
        cancel.check()?;
        let rgb = frame_to_rgb(&frame, cancel)?;
        let buffer = RgbImage::from_raw(frame.width, frame.height, rgb).ok_or_else(|| {
            ComputerError::Backend {
                message: "Linux X11 RGB capture buffer has inconsistent dimensions".into(),
            }
        })?;
        let mut encoded = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(buffer)
            .write_to(&mut encoded, ImageFormat::Png)
            .map_err(|error| ComputerError::Backend {
                message: format!("could not encode Linux X11 screenshot as PNG: {error}"),
            })?;
        cancel.check()?;
        let mut state = state.lock().map_err(|_| ComputerError::Backend {
            message: "Linux X11 computer backend lock is poisoned".into(),
        })?;
        cancel.check()?;
        state.pending_display_size = Some((frame.width, frame.height));
        state.viewport = None;
        Ok(encoded.into_inner())
    }

    async fn capture_png(&self, cancel: &ComputerCancelToken) -> ComputerResult<Vec<u8>> {
        let state = Arc::clone(&self.state);
        let worker_cancel = cancel.clone();
        let mut capture =
            tokio::task::spawn_blocking(move || Self::capture_png_blocking(&state, &worker_cancel));
        tokio::select! {
            biased;
            () = wait_for_cancel(cancel) => {
                capture.abort();
                Err(ComputerError::Cancelled)
            }
            result = &mut capture => result.map_err(|error| ComputerError::Backend {
                message: format!("Linux X11 screenshot worker failed: {error}"),
            })?,
        }
    }

    fn viewport(&self) -> ComputerResult<Viewport> {
        self.lock_state()?
            .viewport
            .ok_or_else(|| ComputerError::InvalidAction {
                message: "take a computer screenshot before sending cursor or control coordinates"
                    .into(),
            })
    }

    fn map_point(&self, point: ScreenPoint) -> ComputerResult<NativePoint> {
        map_delivered_pixel(self.viewport()?, point)
    }

    fn current_native_point(&self) -> ComputerResult<NativePoint> {
        let state = self.lock_state()?;
        let x11 = Self::connection(&state)?;
        let reply = x11
            .connection
            .query_pointer(x11.root)
            .map_err(|error| x11_backend("could not request the X11 cursor position", error))?
            .reply()
            .map_err(|error| x11_backend("could not read the X11 cursor position", error))?;
        if !reply.same_screen {
            return Err(ComputerError::InvalidAction {
                message: "cursor is outside the X11 root window captured by the latest computer screenshot".into(),
            });
        }
        Ok(NativePoint {
            x: reply.root_x,
            y: reply.root_y,
        })
    }

    fn model_cursor_position(&self) -> ComputerResult<(u32, u32)> {
        let viewport = self.viewport()?;
        let point = self.current_native_point()?;
        if point.x < 0
            || point.y < 0
            || u32::from(point.x as u16) >= viewport.display_width
            || u32::from(point.y as u16) >= viewport.display_height
        {
            return Err(ComputerError::InvalidAction {
                message: "cursor is outside the X11 root window captured by the latest computer screenshot".into(),
            });
        }
        let x = u64::from(point.x as u16) * u64::from(viewport.image_width)
            / u64::from(viewport.display_width);
        let y = u64::from(point.y as u16) * u64::from(viewport.image_height)
            / u64::from(viewport.display_height);
        Ok((x as u32, y as u32))
    }

    fn fake_input(
        &self,
        event_type: u8,
        detail: u8,
        root: u32,
        x: i16,
        y: i16,
        context: &str,
    ) -> ComputerResult<()> {
        let state = self.lock_state()?;
        let x11 = Self::connection(&state)?;
        x11.require_xtest()?;
        x11.connection
            .xtest_fake_input(event_type, detail, CURRENT_TIME, root, x, y, 0)
            .map_err(|error| x11_backend(context, error))?
            .check()
            .map_err(|error| x11_backend(context, error))
    }

    fn fake_motion(&self, point: NativePoint) -> ComputerResult<()> {
        let root = {
            let state = self.lock_state()?;
            Self::connection(&state)?.root
        };
        self.fake_input(
            MOTION_NOTIFY_EVENT,
            0,
            root,
            point.x,
            point.y,
            "XTEST rejected a synthetic pointer-motion event",
        )
    }

    fn fake_button(&self, button: u8, is_press: bool) -> ComputerResult<()> {
        self.fake_input(
            if is_press {
                BUTTON_PRESS_EVENT
            } else {
                BUTTON_RELEASE_EVENT
            },
            button,
            0,
            0,
            0,
            "XTEST rejected a synthetic mouse-button event",
        )
    }

    fn fake_key(&self, keycode: KeyCode, is_press: bool) -> ComputerResult<()> {
        self.fake_input(
            if is_press {
                KEY_PRESS_EVENT
            } else {
                KEY_RELEASE_EVENT
            },
            keycode,
            0,
            0,
            0,
            "XTEST rejected a synthetic keyboard event",
        )
    }

    fn held_left_owner() -> ComputerResult<Option<u64>> {
        HELD_LEFT_OWNER
            .lock()
            .map(|owner| *owner)
            .map_err(|_| ComputerError::Backend {
                message: "Linux X11 computer input-owner lock is poisoned".into(),
            })
    }

    fn claim_left_down(&self) -> ComputerResult<()> {
        {
            let mut owner = HELD_LEFT_OWNER.lock().map_err(|_| ComputerError::Backend {
                message: "Linux X11 computer input-owner lock is poisoned".into(),
            })?;
            if owner.is_some_and(|owner| owner != self.input_owner) {
                return Err(ComputerError::Backend {
                    message:
                        "another Haider computer session currently holds the left mouse button"
                            .into(),
                });
            }
            *owner = Some(self.input_owner);
        }
        self.lock_state()?.left_button_down = true;
        Ok(())
    }

    fn clear_left_down(&self) -> ComputerResult<()> {
        {
            let mut owner = HELD_LEFT_OWNER.lock().map_err(|_| ComputerError::Backend {
                message: "Linux X11 computer input-owner lock is poisoned".into(),
            })?;
            if *owner == Some(self.input_owner) {
                *owner = None;
            }
        }
        self.lock_state()?.left_button_down = false;
        Ok(())
    }

    fn post_left_up(&self) -> ComputerResult<()> {
        self.fake_button(BUTTON_LEFT, false)?;
        self.clear_left_down()
    }

    fn click(&self, button: u8, cancel: &ComputerCancelToken) -> ComputerResult<()> {
        cancel.check()?;
        self.fake_button(button, true)?;
        if button == BUTTON_LEFT
            && let Err(error) = self.claim_left_down()
        {
            if self.fake_button(button, false).is_ok() {
                let _ = self.clear_left_down();
            }
            return Err(error);
        }
        let release = || {
            if button == BUTTON_LEFT {
                self.post_left_up()
            } else {
                self.fake_button(button, false)
            }
        };
        let result = cancel.check().and_then(|()| release());
        if result.is_err() {
            let _ = release();
        }
        result
    }

    fn find_key_binding(&self, wanted: KeySym) -> ComputerResult<Option<KeyBinding>> {
        let state = self.lock_state()?;
        find_key_binding(Self::connection(&state)?, wanted)
    }

    fn tap_keysym(
        &self,
        keysym: KeySym,
        requested_modifiers: &[KeySym],
        cancel: &ComputerCancelToken,
    ) -> ComputerResult<()> {
        let binding =
            self.find_key_binding(keysym)?
                .ok_or_else(|| ComputerError::InvalidAction {
                    message: format!(
                        "the active X11 keyboard layout has no key for keysym 0x{keysym:x}"
                    ),
                })?;
        let mut modifiers = Vec::new();
        for modifier in requested_modifiers
            .iter()
            .copied()
            .chain(binding.shift.then_some(XK_SHIFT_L))
        {
            let modifier_binding =
                self.find_key_binding(modifier)?
                    .ok_or_else(|| ComputerError::InvalidAction {
                        message: format!(
                            "the active X11 keyboard layout has no modifier keysym 0x{modifier:x}"
                        ),
                    })?;
            if !modifiers.contains(&modifier_binding.keycode) {
                modifiers.push(modifier_binding.keycode);
            }
        }

        cancel.check()?;
        let mut pressed = Vec::new();
        for keycode in modifiers {
            if let Err(error) = cancel.check().and_then(|()| self.fake_key(keycode, true)) {
                for pressed_keycode in pressed.into_iter().rev() {
                    let _ = self.fake_key(pressed_keycode, false);
                }
                return Err(error);
            }
            pressed.push(keycode);
        }
        if let Err(error) = cancel
            .check()
            .and_then(|()| self.fake_key(binding.keycode, true))
        {
            for keycode in pressed.into_iter().rev() {
                let _ = self.fake_key(keycode, false);
            }
            return Err(error);
        }
        let result = cancel
            .check()
            .and_then(|()| self.fake_key(binding.keycode, false));
        if result.is_err() {
            let _ = self.fake_key(binding.keycode, false);
        }
        let mut release_error = None;
        for keycode in pressed.into_iter().rev() {
            if let Err(error) = self.fake_key(keycode, false) {
                // A modifier-down crossed the server boundary. Mirror mouse
                // cleanup by making one best-effort second release attempt.
                let _ = self.fake_key(keycode, false);
                if release_error.is_none() {
                    release_error = Some(error);
                }
            }
        }
        result.and_then(|()| release_error.map_or(Ok(()), Err))
    }

    async fn type_text(&self, text: &str, cancel: &ComputerCancelToken) -> ComputerResult<()> {
        for (index, scalar) in text.chars().enumerate() {
            cancel.check()?;
            self.tap_keysym(keysym_for_char(scalar), &[], cancel)?;
            if index % 32 == 31 {
                tokio::task::yield_now().await;
            }
        }
        cancel.check()
    }

    fn press_key(&self, keys: &str, cancel: &ComputerCancelToken) -> ComputerResult<()> {
        let mut parts = keys
            .split('+')
            .map(str::trim)
            .filter(|part| !part.is_empty());
        let Some(last) = parts.next_back() else {
            return Err(ComputerError::InvalidAction {
                message: "keyboard shortcut is empty".into(),
            });
        };
        let mut modifiers = Vec::new();
        for modifier in parts {
            let keysym = match modifier.to_ascii_lowercase().as_str() {
                "cmd" | "command" | "meta" | "super" => XK_SUPER_L,
                "shift" => XK_SHIFT_L,
                "ctrl" | "control" => XK_CONTROL_L,
                "alt" | "option" => XK_ALT_L,
                unknown => {
                    return Err(ComputerError::InvalidAction {
                        message: format!("unsupported Linux X11 key modifier `{unknown}`"),
                    });
                }
            };
            if !modifiers.contains(&keysym) {
                modifiers.push(keysym);
            }
        }
        let keysym = keysym_for_key_name(last).ok_or_else(|| ComputerError::InvalidAction {
            message: format!("unsupported Linux X11 key `{last}`"),
        })?;
        self.tap_keysym(keysym, &modifiers, cancel)
    }

    async fn scroll(
        &self,
        point: NativePoint,
        direction: ScrollDirection,
        amount: u32,
        cancel: &ComputerCancelToken,
    ) -> ComputerResult<()> {
        self.fake_motion(point)?;
        let button = match direction {
            ScrollDirection::Up => BUTTON_SCROLL_UP,
            ScrollDirection::Down => BUTTON_SCROLL_DOWN,
            ScrollDirection::Left => BUTTON_SCROLL_LEFT,
            ScrollDirection::Right => BUTTON_SCROLL_RIGHT,
        };
        for index in 0..amount {
            self.click(button, cancel)?;
            if index % 32 == 31 {
                tokio::task::yield_now().await;
            }
        }
        Ok(())
    }

    async fn control(
        &self,
        action: &ComputerAction,
        cancel: &ComputerCancelToken,
    ) -> ComputerResult<ComputerOutput> {
        self.ensure_control_available()?;
        let _input_gate = INPUT_GATE.lock().await;
        cancel.check()?;
        let held_owner = Self::held_left_owner()?;
        if held_owner.is_some_and(|owner| owner != self.input_owner) {
            return Err(ComputerError::Backend {
                message: "another Haider computer session currently holds the left mouse button"
                    .into(),
            });
        }
        let left_held_by_self = held_owner == Some(self.input_owner);
        if left_held_by_self && !action_allowed_while_left_held(action) {
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

        match action {
            ComputerAction::LeftClick { x, y } => {
                self.fake_motion(self.map_point(ScreenPoint { x: *x, y: *y })?)?;
                self.click(BUTTON_LEFT, cancel)?;
            }
            ComputerAction::RightClick => self.click(BUTTON_RIGHT, cancel)?,
            ComputerAction::MiddleClick => self.click(BUTTON_MIDDLE, cancel)?,
            ComputerAction::DoubleClick => {
                self.click(BUTTON_LEFT, cancel)?;
                self.click(BUTTON_LEFT, cancel)?;
            }
            ComputerAction::LeftMouseDown => {
                self.fake_button(BUTTON_LEFT, true)?;
                if let Err(error) = self.claim_left_down() {
                    if self.fake_button(BUTTON_LEFT, false).is_ok() {
                        let _ = self.clear_left_down();
                    }
                    return Err(error);
                }
            }
            ComputerAction::LeftMouseUp => self.post_left_up()?,
            ComputerAction::MouseMove { x, y } => {
                self.fake_motion(self.map_point(ScreenPoint { x: *x, y: *y })?)?;
            }
            ComputerAction::LeftClickDrag { from, to } => {
                let from = self.map_point(*from)?;
                let to = self.map_point(*to)?;
                self.fake_motion(from)?;
                cancel.check()?;
                self.fake_button(BUTTON_LEFT, true)?;
                if let Err(error) = self.claim_left_down() {
                    if self.fake_button(BUTTON_LEFT, false).is_ok() {
                        let _ = self.clear_left_down();
                    }
                    return Err(error);
                }
                let result = cancel
                    .check()
                    .and_then(|()| self.fake_motion(to))
                    .and_then(|()| self.post_left_up());
                if result.is_err() {
                    let _ = self.post_left_up();
                }
                result?;
            }
            ComputerAction::Type { text } => self.type_text(text, cancel).await?,
            ComputerAction::Key { keys } => self.press_key(keys, cancel)?,
            ComputerAction::Scroll {
                x,
                y,
                direction,
                amount,
            } => {
                self.scroll(
                    self.map_point(ScreenPoint { x: *x, y: *y })?,
                    *direction,
                    *amount,
                    cancel,
                )
                .await?;
            }
            ComputerAction::Wait { .. } => unreachable!("wait is dispatched asynchronously"),
            ComputerAction::Screenshot | ComputerAction::CursorPosition => {
                unreachable!("observe actions do not enter control")
            }
        }
        Ok(ComputerOutput::Confirmed {
            action: action_name(action).into(),
        })
    }
}

#[async_trait]
impl ComputerBackend for LinuxComputerBackend {
    async fn execute(
        &self,
        action: &ComputerAction,
        cancel: &ComputerCancelToken,
    ) -> ComputerResult<ComputerOutput> {
        cancel.check()?;
        self.ensure_connection(cancel).await?;
        match action {
            ComputerAction::Screenshot => self
                .capture_png(cancel)
                .await
                .map(ComputerOutput::ScreenshotPng),
            ComputerAction::CursorPosition => {
                let (x, y) = self.model_cursor_position()?;
                Ok(ComputerOutput::CursorPosition { x, y })
            }
            ComputerAction::Wait { ms } => {
                self.ensure_control_available()?;
                self.viewport()?;
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_millis(*ms)) => {
                        Ok(ComputerOutput::Confirmed { action: "wait".into() })
                    }
                    () = wait_for_cancel(cancel) => Err(ComputerError::Cancelled),
                }
            }
            _ => self.control(action, cancel).await,
        }
    }

    fn set_viewport(&self, width: u32, height: u32) -> ComputerResult<()> {
        if width == 0 || height == 0 {
            return Err(ComputerError::InvalidAction {
                message: "CU-1 returned an empty computer screenshot viewport".into(),
            });
        }
        let mut state = self.lock_state()?;
        let (display_width, display_height) =
            state
                .pending_display_size
                .ok_or_else(|| ComputerError::Backend {
                    message: "CU-1 viewport arrived without a matching Linux X11 capture".into(),
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
        let _input_gate = INPUT_GATE.lock().await;
        if Self::held_left_owner()? == Some(self.input_owner) {
            self.post_left_up()?;
        }
        Ok(())
    }
}

fn x11_backend(context: &str, error: impl std::fmt::Display) -> ComputerError {
    ComputerError::Backend {
        message: format!("{context}: {error}"),
    }
}

fn frame_to_rgb(frame: &CaptureFrame, cancel: &ComputerCancelToken) -> ComputerResult<Vec<u8>> {
    if frame.red_mask == 0 || frame.green_mask == 0 || frame.blue_mask == 0 {
        return Err(ComputerError::Backend {
            message: "the X11 root visual is not direct RGB; use a TrueColor X server (Xvfb: -screen 0 <width>x<height>x24)".into(),
        });
    }
    let bytes_per_pixel = match frame.bits_per_pixel {
        8 => 1_usize,
        16 => 2,
        24 => 3,
        32 => 4,
        other => {
            return Err(ComputerError::Backend {
                message: format!("unsupported X11 root pixel width: {other} bits per pixel"),
            });
        }
    };
    let width = frame.width as usize;
    let height = frame.height as usize;
    let scanline_pad = usize::from(frame.scanline_pad);
    if !matches!(scanline_pad, 8 | 16 | 32) {
        return Err(ComputerError::Backend {
            message: format!(
                "unsupported X11 screenshot scanline padding: {} bits",
                frame.scanline_pad
            ),
        });
    }
    let row_bits = width
        .checked_mul(usize::from(frame.bits_per_pixel))
        .ok_or_else(|| ComputerError::Backend {
            message: "X11 screenshot row size overflow".into(),
        })?;
    let stride_bits = row_bits
        .checked_add(scanline_pad - 1)
        .map(|bits| bits / scanline_pad * scanline_pad)
        .ok_or_else(|| ComputerError::Backend {
            message: "X11 screenshot padded row size overflow".into(),
        })?;
    let stride = stride_bits / 8;
    let pixel_bytes = width
        .checked_mul(bytes_per_pixel)
        .ok_or_else(|| ComputerError::Backend {
            message: "X11 screenshot pixel row size overflow".into(),
        })?;
    if stride < pixel_bytes {
        return Err(ComputerError::Backend {
            message: "X11 screenshot row stride is shorter than its pixel data".into(),
        });
    }
    let payload_len = stride
        .checked_mul(height)
        .ok_or_else(|| ComputerError::Backend {
            message: "X11 screenshot buffer size overflow".into(),
        })?;
    let wire_len = payload_len
        .checked_add(3)
        .map(|length| length & !3)
        .ok_or_else(|| ComputerError::Backend {
            message: "X11 screenshot wire size overflow".into(),
        })?;
    if frame.data.len() != wire_len {
        return Err(ComputerError::Backend {
            message: "X11 screenshot buffer has an inconsistent row or wire-padding layout".into(),
        });
    }
    let output_len = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(3))
        .ok_or_else(|| ComputerError::Backend {
            message: "Linux screenshot RGB buffer size overflow".into(),
        })?;
    let mut rgb = Vec::with_capacity(output_len);
    for y in 0..height {
        if y % 64 == 0 {
            cancel.check()?;
        }
        let row = &frame.data[y * stride..(y + 1) * stride];
        for x in 0..width {
            let start = x * bytes_per_pixel;
            let pixel_bytes = &row[start..start + bytes_per_pixel];
            let pixel = if frame.byte_order == ImageOrder::LSB_FIRST {
                pixel_bytes
                    .iter()
                    .enumerate()
                    .fold(0_u64, |value, (index, byte)| {
                        value | (u64::from(*byte) << (index * 8))
                    })
            } else {
                pixel_bytes
                    .iter()
                    .fold(0_u64, |value, byte| (value << 8) | u64::from(*byte))
            };
            rgb.push(mask_component(pixel, u64::from(frame.red_mask)));
            rgb.push(mask_component(pixel, u64::from(frame.green_mask)));
            rgb.push(mask_component(pixel, u64::from(frame.blue_mask)));
        }
    }
    Ok(rgb)
}

fn mask_component(pixel: u64, mask: u64) -> u8 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    let maximum = mask >> shift;
    let value = (pixel & mask) >> shift;
    ((value * 255 + maximum / 2) / maximum) as u8
}

fn find_key_binding(
    connection: &X11Connection,
    wanted: KeySym,
) -> ComputerResult<Option<KeyBinding>> {
    let setup = connection.connection.setup();
    let minimum = setup.min_keycode;
    let count = setup
        .max_keycode
        .checked_sub(minimum)
        .and_then(|difference| difference.checked_add(1))
        .ok_or_else(|| ComputerError::Backend {
            message: "X11 keyboard map reported an invalid keycode range".into(),
        })?;
    let reply = connection
        .connection
        .get_keyboard_mapping(minimum, count)
        .map_err(|error| x11_backend("could not request the active X11 keyboard map", error))?
        .reply()
        .map_err(|error| x11_backend("could not read the active X11 keyboard map", error))?;
    let levels = usize::from(reply.keysyms_per_keycode);
    if levels == 0 || reply.keysyms.len() != usize::from(count) * levels {
        return Err(ComputerError::Backend {
            message: "X11 keyboard map returned inconsistent dimensions".into(),
        });
    }
    for offset in 0..usize::from(count) {
        let base = offset * levels;
        for level in 0..usize::min(levels, 2) {
            if reply.keysyms[base + level] == wanted {
                return Ok(Some(KeyBinding {
                    keycode: minimum + offset as u8,
                    shift: level == 1,
                }));
            }
        }
        // Some compact maps leave level 1 as NoSymbol and rely on the core
        // alphabetic case rule. Preserve uppercase typing for that legal map.
        if (u32::from(b'A')..=u32::from(b'Z')).contains(&wanted)
            && reply.keysyms[base] == wanted + u32::from(b'a' - b'A')
            && (levels == 1 || reply.keysyms[base + 1] == 0)
        {
            return Ok(Some(KeyBinding {
                keycode: minimum + offset as u8,
                shift: true,
            }));
        }
    }
    Ok(None)
}

fn map_delivered_pixel(viewport: Viewport, point: ScreenPoint) -> ComputerResult<NativePoint> {
    if point.x >= viewport.image_width || point.y >= viewport.image_height {
        return Err(ComputerError::InvalidAction {
            message: format!(
                "computer coordinate ({}, {}) is outside the delivered {}x{} screenshot",
                point.x, point.y, viewport.image_width, viewport.image_height
            ),
        });
    }
    let x =
        u64::from(point.x) * u64::from(viewport.display_width) / u64::from(viewport.image_width);
    let y =
        u64::from(point.y) * u64::from(viewport.display_height) / u64::from(viewport.image_height);
    let x = i16::try_from(x).map_err(|_| ComputerError::InvalidAction {
        message: "mapped X11 x coordinate exceeds the core event range".into(),
    })?;
    let y = i16::try_from(y).map_err(|_| ComputerError::InvalidAction {
        message: "mapped X11 y coordinate exceeds the core event range".into(),
    })?;
    Ok(NativePoint { x, y })
}

fn keysym_for_char(scalar: char) -> KeySym {
    match scalar {
        '\n' | '\r' => XK_RETURN,
        '\t' => XK_TAB,
        '\u{8}' => XK_BACK_SPACE,
        scalar if scalar as u32 <= 0xff => scalar as KeySym,
        scalar => 0x0100_0000 | scalar as KeySym,
    }
}

fn keysym_for_key_name(key: &str) -> Option<KeySym> {
    if key.chars().count() == 1 {
        return key.chars().next().map(keysym_for_char);
    }
    Some(match key.to_ascii_lowercase().as_str() {
        "backspace" => XK_BACK_SPACE,
        "tab" => XK_TAB,
        "return" | "enter" => XK_RETURN,
        "escape" | "esc" => XK_ESCAPE,
        "home" => XK_HOME,
        "left" => XK_LEFT,
        "up" => XK_UP,
        "right" => XK_RIGHT,
        "down" => XK_DOWN,
        "pageup" | "page_up" => XK_PAGE_UP,
        "pagedown" | "page_down" => XK_PAGE_DOWN,
        "end" => XK_END,
        "delete" => XK_DELETE,
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

fn action_allowed_while_left_held(action: &ComputerAction) -> bool {
    matches!(
        action,
        ComputerAction::LeftMouseUp | ComputerAction::MouseMove { .. }
    )
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

async fn wait_for_cancel(cancel: &ComputerCancelToken) {
    while !cancel.is_cancelled() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivered_cu1_pixels_map_to_x11_root_without_assuming_native_size() {
        let viewport = Viewport {
            display_width: 3_200,
            display_height: 1_800,
            image_width: 1_600,
            image_height: 900,
        };
        let point = match map_delivered_pixel(viewport, ScreenPoint { x: 800, y: 450 }) {
            Ok(point) => point,
            Err(error) => panic!("center must map: {error}"),
        };
        assert_eq!(point.x, 1_600);
        assert_eq!(point.y, 900);
        assert!(map_delivered_pixel(viewport, ScreenPoint { x: 1_600, y: 0 }).is_err());
    }

    #[test]
    fn x11_masks_scale_color_channels_to_rgb8() {
        assert_eq!(mask_component(0x00ff_0000, 0x00ff_0000), 255);
        assert_eq!(mask_component(0x0000_8000, 0x0000_ff00), 128);
        assert_eq!(mask_component(0x0000_001f, 0x0000_001f), 255);
        assert_eq!(mask_component(0, 0), 0);
    }

    #[test]
    fn x11_scanline_stride_ignores_only_final_wire_padding() {
        let cancel = ComputerCancelToken::new();
        let frame = |scanline_pad, data| CaptureFrame {
            width: 1,
            height: 2,
            bits_per_pixel: 8,
            scanline_pad,
            byte_order: ImageOrder::LSB_FIRST,
            red_mask: 0xe0,
            green_mask: 0x1c,
            blue_mask: 0x03,
            data,
        };
        let pad8 = frame(8, vec![0xe0, 0x03, 0, 0]);
        assert_eq!(frame_to_rgb(&pad8, &cancel), Ok(vec![255, 0, 0, 0, 0, 255]));
        let pad16 = frame(16, vec![0xe0, 0, 0x03, 0]);
        assert_eq!(
            frame_to_rgb(&pad16, &cancel),
            Ok(vec![255, 0, 0, 0, 0, 255])
        );
    }

    #[test]
    fn linux_key_names_and_unicode_follow_x11_keysym_rules() {
        assert_eq!(keysym_for_char('A'), u32::from(b'A'));
        assert_eq!(keysym_for_char('\u{20ac}'), 0x0100_20ac);
        assert_eq!(keysym_for_key_name("enter"), Some(XK_RETURN));
        assert_eq!(keysym_for_key_name("F12"), Some(0xffc9));
        assert_eq!(keysym_for_key_name("not-a-key"), None);
    }

    #[test]
    fn held_button_only_allows_move_or_release() {
        assert!(action_allowed_while_left_held(&ComputerAction::MouseMove {
            x: 1,
            y: 2
        }));
        assert!(action_allowed_while_left_held(&ComputerAction::LeftMouseUp));
        assert!(!action_allowed_while_left_held(
            &ComputerAction::LeftClick { x: 1, y: 2 }
        ));
    }
}
