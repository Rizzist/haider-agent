//! Windows GDI/SendInput backend.
//!
//! The process requests per-monitor-v2 DPI awareness before using screen APIs.
//! In that mode, GDI virtual-screen metrics, captured pixels, `GetCursorPos`,
//! and `SendInput`'s `MOUSEEVENTF_VIRTUALDESK` coordinates share the physical
//! virtual-desktop space. CU-1 still owns image admission and downscaling:
//! [`ComputerBackend::set_viewport`] records the dimensions delivered to the
//! model, and model pixels map back with
//! `floor(model_pixel * virtual_extent / admitted_extent)` before conversion
//! to SendInput's inclusive 0..=65535 absolute range.
//!
//! Windows has no TCC-style prompt. Screen capture and input do require an
//! interactive desktop (services in session 0 do not have one). Windows UIPI
//! also blocks input sent to an elevated foreground window unless Haider is
//! elevated to the same integrity level. Failures are returned as actionable
//! [`ComputerError::Unavailable`] values instead of becoming silent no-ops.
//!
//! GitHub `windows-latest` runners do not provide the interactive desktop
//! needed for a real GDI/SendInput round trip. CI coverage is therefore the
//! cross-platform fake backend plus Windows compile/clippy; the ignored test
//! at the bottom of this module is for manual validation on a Windows desktop.

use super::{ComputerBackend, ComputerCancelToken, ComputerError, ComputerOutput, ComputerResult};
use async_trait::async_trait;
use haider_protocol::computer::{ComputerAction, ScreenPoint, ScrollDirection};
use image::{DynamicImage, ImageFormat, RgbImage};
use std::io::Cursor;
use std::mem::{size_of, size_of_val};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, Once};
use std::time::Duration;
use windows_sys::Win32::Foundation::{POINT, SetLastError};
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BitBlt, CAPTUREBLT, CreateCompatibleBitmap, CreateCompatibleDC,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, HBITMAP, HDC, HGDIOBJ, ReleaseDC,
    SRCCOPY, SelectObject,
};
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
    MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput, VK_BACK, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END,
    VK_ESCAPE, VK_F1, VK_HOME, VK_LEFT, VK_LWIN, VK_MENU, VK_NEXT, VK_PRIOR, VK_RETURN, VK_RIGHT,
    VK_SHIFT, VK_SPACE, VK_TAB, VK_UP, VkKeyScanW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN,
};

const WHEEL_DELTA: i32 = 120;
const ABSOLUTE_MAX: u64 = 65_535;

static DPI_AWARENESS: Once = Once::new();
static NEXT_INPUT_OWNER: AtomicU64 = AtomicU64::new(1);
static INPUT_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static HELD_LEFT_OWNER: Mutex<Option<u64>> = Mutex::new(None);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VirtualScreen {
    left: i32,
    top: i32,
    width: u32,
    height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NativePoint {
    x: i32,
    y: i32,
}

#[derive(Debug, Clone, Copy)]
struct Viewport {
    screen: VirtualScreen,
    image_width: u32,
    image_height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KeySpec {
    virtual_key: u16,
    extended: bool,
}

#[derive(Debug, Default)]
struct BackendState {
    pending_screen: Option<VirtualScreen>,
    viewport: Option<Viewport>,
    left_button_down: bool,
}

#[derive(Debug)]
pub(crate) struct WindowsComputerBackend {
    input_owner: u64,
    state: Mutex<BackendState>,
}

impl WindowsComputerBackend {
    pub(crate) fn new() -> Self {
        ensure_dpi_awareness();
        Self {
            input_owner: NEXT_INPUT_OWNER.fetch_add(1, Ordering::Relaxed),
            state: Mutex::new(BackendState::default()),
        }
    }

    fn lock_state(&self) -> ComputerResult<MutexGuard<'_, BackendState>> {
        self.state.lock().map_err(|_| ComputerError::Backend {
            message: "Windows computer backend lock is poisoned".into(),
        })
    }

    fn virtual_screen() -> ComputerResult<VirtualScreen> {
        ensure_dpi_awareness();
        // SAFETY: GetSystemMetrics takes value-only metric identifiers and has
        // no pointer or ownership requirements.
        let (left, top, width, height) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };
        if width <= 0 || height <= 0 {
            return Err(no_interactive_desktop(format!(
                "Windows reported an empty virtual screen ({width}x{height})"
            )));
        }
        Ok(VirtualScreen {
            left,
            top,
            width: width as u32,
            height: height as u32,
        })
    }

    fn capture_png_blocking(
        cancel: &ComputerCancelToken,
    ) -> ComputerResult<(Vec<u8>, VirtualScreen)> {
        cancel.check()?;
        let screen = Self::virtual_screen()?;
        let width = i32::try_from(screen.width).map_err(|_| ComputerError::Backend {
            message: "Windows virtual-screen width exceeds the GDI capture range".into(),
        })?;
        let height = i32::try_from(screen.height).map_err(|_| ComputerError::Backend {
            message: "Windows virtual-screen height exceeds the GDI capture range".into(),
        })?;

        // Declaration order is deliberate. Reverse-order drops destroy the
        // memory DC before its bitmap and release the screen DC last. The
        // selection guard restores the original object before either drops.
        // SAFETY: a null HWND requests the desktop screen DC.
        let screen_dc = ScreenDc(unsafe { GetDC(ptr::null_mut()) });
        if screen_dc.0.is_null() {
            return Err(last_windows_unavailable(
                "could not acquire the Windows virtual-screen device context",
            ));
        }
        // SAFETY: the source DC is live and dimensions were validated above.
        let bitmap = OwnedBitmap(unsafe { CreateCompatibleBitmap(screen_dc.0, width, height) });
        if bitmap.0.is_null() {
            return Err(last_windows_unavailable(
                "could not create the Windows screenshot bitmap",
            ));
        }
        // SAFETY: the source DC remains live for the memory DC's lifetime.
        let memory_dc = MemoryDc(unsafe { CreateCompatibleDC(screen_dc.0) });
        if memory_dc.0.is_null() {
            return Err(last_windows_unavailable(
                "could not create the Windows screenshot memory device context",
            ));
        }
        let selected = SelectedBitmap::new(&memory_dc, &bitmap)?;
        cancel.check()?;
        // SAFETY: both DCs and the selected compatible bitmap are live. The
        // destination begins at (0,0); the source origin may be negative on a
        // multi-monitor virtual desktop. CAPTUREBLT includes layered windows.
        if unsafe {
            BitBlt(
                memory_dc.0,
                0,
                0,
                width,
                height,
                screen_dc.0,
                screen.left,
                screen.top,
                SRCCOPY | CAPTUREBLT,
            )
        } == 0
        {
            return Err(last_windows_unavailable(
                "could not copy pixels from the Windows virtual screen",
            ));
        }
        cancel.check()?;
        // GetDIBits requires that the queried bitmap is not selected into a
        // device context. This consumes the guard and restores the old object.
        selected.restore()?;

        let pixel_count = usize::try_from(screen.width)
            .ok()
            .and_then(|width| {
                usize::try_from(screen.height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .ok_or_else(|| ComputerError::Backend {
                message: "Windows screen dimensions overflow the capture buffer".into(),
            })?;
        let bgra_len = pixel_count
            .checked_mul(4)
            .ok_or_else(|| ComputerError::Backend {
                message: "Windows screen dimensions overflow the BGRA capture buffer".into(),
            })?;
        let image_size = u32::try_from(bgra_len).map_err(|_| ComputerError::Backend {
            message: "Windows BGRA capture buffer exceeds the GDI image-size range".into(),
        })?;
        let mut bitmap_info = BITMAPINFO::default();
        bitmap_info.bmiHeader.biSize =
            u32::try_from(size_of_val(&bitmap_info.bmiHeader)).map_err(|_| {
                ComputerError::Backend {
                    message: "Windows bitmap header size exceeds the GDI range".into(),
                }
            })?;
        bitmap_info.bmiHeader.biWidth = width;
        bitmap_info.bmiHeader.biHeight = -height;
        bitmap_info.bmiHeader.biPlanes = 1;
        bitmap_info.bmiHeader.biBitCount = 32;
        bitmap_info.bmiHeader.biCompression = BI_RGB;
        bitmap_info.bmiHeader.biSizeImage = image_size;
        let mut bgra = vec![0_u8; bgra_len];
        // SAFETY: the bitmap is live and no longer selected. `bgra` exactly
        // holds width*height top-down BGRA pixels described by bitmap_info.
        let copied_lines = unsafe {
            GetDIBits(
                screen_dc.0,
                bitmap.0,
                0,
                screen.height,
                bgra.as_mut_ptr().cast(),
                &raw mut bitmap_info,
                DIB_RGB_COLORS,
            )
        };
        if copied_lines != height {
            return Err(last_windows_unavailable(&format!(
                "could not read the Windows screenshot bitmap (copied {copied_lines} of {height} rows)"
            )));
        }
        cancel.check()?;

        let rgb_len = pixel_count
            .checked_mul(3)
            .ok_or_else(|| ComputerError::Backend {
                message: "Windows screen dimensions overflow the RGB capture buffer".into(),
            })?;
        let row_bytes = usize::try_from(screen.width)
            .ok()
            .and_then(|width| width.checked_mul(4))
            .ok_or_else(|| ComputerError::Backend {
                message: "Windows screenshot row size overflow".into(),
            })?;
        let mut rgb = Vec::with_capacity(rgb_len);
        for (row_index, row) in bgra.chunks_exact(row_bytes).enumerate() {
            if row_index % 64 == 0 {
                cancel.check()?;
            }
            for pixel in row.chunks_exact(4) {
                rgb.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
            }
        }
        let buffer = RgbImage::from_raw(screen.width, screen.height, rgb).ok_or_else(|| {
            ComputerError::Backend {
                message: "Windows RGB capture buffer has inconsistent dimensions".into(),
            }
        })?;
        let mut encoded = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(buffer)
            .write_to(&mut encoded, ImageFormat::Png)
            .map_err(|error| ComputerError::Backend {
                message: format!("could not encode Windows screenshot as PNG: {error}"),
            })?;
        cancel.check()?;
        Ok((encoded.into_inner(), screen))
    }

    async fn capture_png(&self, cancel: &ComputerCancelToken) -> ComputerResult<Vec<u8>> {
        let worker_cancel = cancel.clone();
        let mut capture =
            tokio::task::spawn_blocking(move || Self::capture_png_blocking(&worker_cancel));
        let (png, screen) = tokio::select! {
            biased;
            () = wait_for_cancel(cancel) => {
                capture.abort();
                return Err(ComputerError::Cancelled);
            }
            result = &mut capture => result.map_err(|error| ComputerError::Backend {
                message: format!("Windows screenshot worker failed: {error}"),
            })??,
        };
        cancel.check()?;
        let mut state = self.lock_state()?;
        state.pending_screen = Some(screen);
        state.viewport = None;
        Ok(png)
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

    fn current_native_point() -> ComputerResult<NativePoint> {
        ensure_dpi_awareness();
        let mut point = POINT::default();
        // SAFETY: point is valid writable storage for the duration of the call.
        if unsafe { GetCursorPos(&raw mut point) } == 0 {
            return Err(last_windows_unavailable(
                "could not read the Windows cursor position",
            ));
        }
        Ok(NativePoint {
            x: point.x,
            y: point.y,
        })
    }

    fn model_cursor_position(&self) -> ComputerResult<(u32, u32)> {
        let viewport = self.viewport()?;
        let point = Self::current_native_point()?;
        let relative_x = i64::from(point.x) - i64::from(viewport.screen.left);
        let relative_y = i64::from(point.y) - i64::from(viewport.screen.top);
        if relative_x < 0
            || relative_y < 0
            || relative_x >= i64::from(viewport.screen.width)
            || relative_y >= i64::from(viewport.screen.height)
        {
            return Err(ComputerError::InvalidAction {
                message:
                    "cursor is outside the virtual screen captured by the latest computer screenshot"
                        .into(),
            });
        }
        let x = u64::try_from(relative_x).map_err(|_| ComputerError::Backend {
            message: "Windows cursor X coordinate conversion failed".into(),
        })? * u64::from(viewport.image_width)
            / u64::from(viewport.screen.width);
        let y = u64::try_from(relative_y).map_err(|_| ComputerError::Backend {
            message: "Windows cursor Y coordinate conversion failed".into(),
        })? * u64::from(viewport.image_height)
            / u64::from(viewport.screen.height);
        Ok((x as u32, y as u32))
    }

    fn send_mouse(flags: u32, dx: i32, dy: i32, data: u32, context: &str) -> ComputerResult<()> {
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: data,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        send_input(&input, context)
    }

    fn move_mouse(&self, point: NativePoint) -> ComputerResult<()> {
        let viewport = self.viewport()?;
        let (dx, dy) = normalize_absolute(viewport.screen, point)?;
        Self::send_mouse(
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            dx,
            dy,
            0,
            "Windows rejected a synthetic pointer-motion event",
        )
    }

    fn send_button(flags: u32) -> ComputerResult<()> {
        Self::send_mouse(
            flags,
            0,
            0,
            0,
            "Windows rejected a synthetic mouse-button event",
        )
    }

    fn held_left_owner() -> ComputerResult<Option<u64>> {
        HELD_LEFT_OWNER
            .lock()
            .map(|owner| *owner)
            .map_err(|_| ComputerError::Backend {
                message: "Windows computer input-owner lock is poisoned".into(),
            })
    }

    fn claim_left_down(&self) -> ComputerResult<()> {
        {
            let mut owner = HELD_LEFT_OWNER.lock().map_err(|_| ComputerError::Backend {
                message: "Windows computer input-owner lock is poisoned".into(),
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
                message: "Windows computer input-owner lock is poisoned".into(),
            })?;
            if *owner == Some(self.input_owner) {
                *owner = None;
            }
        }
        self.lock_state()?.left_button_down = false;
        Ok(())
    }

    fn post_left_up(&self) -> ComputerResult<()> {
        Self::send_button(MOUSEEVENTF_LEFTUP)?;
        self.clear_left_down()
    }

    fn click(
        &self,
        down: u32,
        up: u32,
        is_left: bool,
        cancel: &ComputerCancelToken,
    ) -> ComputerResult<()> {
        cancel.check()?;
        Self::send_button(down)?;
        if is_left && let Err(error) = self.claim_left_down() {
            if Self::send_button(up).is_ok() {
                let _ = self.clear_left_down();
            }
            return Err(error);
        }
        let release = || {
            if is_left {
                self.post_left_up()
            } else {
                Self::send_button(up)
            }
        };
        let result = cancel.check().and_then(|()| release());
        if result.is_err() {
            let _ = release();
        }
        result
    }

    fn post_key(spec: KeySpec, key_up: bool) -> ComputerResult<()> {
        let mut flags = if spec.extended {
            KEYEVENTF_EXTENDEDKEY
        } else {
            0
        };
        if key_up {
            flags |= KEYEVENTF_KEYUP;
        }
        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: spec.virtual_key,
                    wScan: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        send_input(&input, "Windows rejected a synthetic keyboard event")
    }

    fn unicode_input(unit: u16, key_up: bool) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: 0,
                    wScan: unit,
                    dwFlags: KEYEVENTF_UNICODE | if key_up { KEYEVENTF_KEYUP } else { 0 },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    fn unicode_scalar_inputs(scalar: char) -> Vec<INPUT> {
        let mut encoded = [0_u16; 2];
        let units = scalar.encode_utf16(&mut encoded);
        units
            .iter()
            .copied()
            .map(|unit| Self::unicode_input(unit, false))
            .chain(
                units
                    .iter()
                    .copied()
                    .map(|unit| Self::unicode_input(unit, true)),
            )
            .collect()
    }

    fn unicode_scalar_keyups(scalar: char) -> Vec<INPUT> {
        let mut encoded = [0_u16; 2];
        scalar
            .encode_utf16(&mut encoded)
            .iter()
            .copied()
            .map(|unit| Self::unicode_input(unit, true))
            .collect()
    }

    fn type_scalar(scalar: char, cancel: &ComputerCancelToken) -> ComputerResult<()> {
        let inputs = Self::unicode_scalar_inputs(scalar);
        cancel.check()?;
        let result = send_inputs(&inputs, "Windows rejected a Unicode keyboard event");
        if result.is_err() {
            // SendInput reports a prefix count on partial insertion. Release
            // every code unit defensively; duplicate Unicode key-up events are
            // harmless and avoid retaining half of a surrogate-pair stroke.
            let cleanup = Self::unicode_scalar_keyups(scalar);
            let _ = send_inputs(&cleanup, "Windows rejected Unicode key cleanup");
        }
        result?;
        cancel.check()
    }

    async fn type_text(&self, text: &str, cancel: &ComputerCancelToken) -> ComputerResult<()> {
        for (index, scalar) in text.chars().enumerate() {
            Self::type_scalar(scalar, cancel)?;
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
            let spec = modifier_key(modifier).ok_or_else(|| ComputerError::InvalidAction {
                message: format!("unsupported Windows key modifier `{modifier}`"),
            })?;
            push_unique(&mut modifiers, spec);
        }
        let (key, implied_modifiers) = key_binding(last)?;
        for modifier in implied_modifiers {
            push_unique(&mut modifiers, modifier);
        }

        cancel.check()?;
        let mut pressed = Vec::new();
        for modifier in modifiers {
            if let Err(error) = cancel
                .check()
                .and_then(|()| Self::post_key(modifier, false))
            {
                release_keys(&pressed);
                return Err(error);
            }
            pressed.push(modifier);
        }
        if let Err(error) = cancel.check().and_then(|()| Self::post_key(key, false)) {
            release_keys(&pressed);
            return Err(error);
        }
        let result = cancel.check().and_then(|()| Self::post_key(key, true));
        if result.is_err() {
            let _ = Self::post_key(key, true);
        }
        let mut release_error = None;
        for modifier in pressed.into_iter().rev() {
            if let Err(error) = Self::post_key(modifier, true) {
                let _ = Self::post_key(modifier, true);
                if release_error.is_none() {
                    release_error = Some(error);
                }
            }
        }
        result.and_then(|()| release_error.map_or(Ok(()), Err))
    }

    fn scroll(
        &self,
        point: NativePoint,
        direction: ScrollDirection,
        amount: u32,
        cancel: &ComputerCancelToken,
    ) -> ComputerResult<()> {
        cancel.check()?;
        self.move_mouse(point)?;
        cancel.check()?;
        let magnitude = i32::try_from(amount)
            .ok()
            .and_then(|amount| amount.checked_mul(WHEEL_DELTA))
            .ok_or_else(|| ComputerError::InvalidAction {
                message: "computer scroll amount exceeds the Windows wheel-event range".into(),
            })?;
        let (flags, delta) = match direction {
            ScrollDirection::Up => (MOUSEEVENTF_WHEEL, magnitude),
            ScrollDirection::Down => (MOUSEEVENTF_WHEEL, -magnitude),
            ScrollDirection::Left => (MOUSEEVENTF_HWHEEL, -magnitude),
            ScrollDirection::Right => (MOUSEEVENTF_HWHEEL, magnitude),
        };
        Self::send_mouse(
            flags,
            0,
            0,
            delta as u32,
            "Windows rejected a synthetic wheel event",
        )
    }

    async fn control(
        &self,
        action: &ComputerAction,
        cancel: &ComputerCancelToken,
    ) -> ComputerResult<ComputerOutput> {
        ensure_dpi_awareness();
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
                self.move_mouse(self.map_point(ScreenPoint { x: *x, y: *y })?)?;
                self.click(MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, true, cancel)?;
            }
            ComputerAction::RightClick => {
                self.click(MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, false, cancel)?
            }
            ComputerAction::MiddleClick => {
                self.click(MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, false, cancel)?
            }
            ComputerAction::DoubleClick => {
                self.click(MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, true, cancel)?;
                self.click(MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, true, cancel)?;
            }
            ComputerAction::LeftMouseDown => {
                Self::send_button(MOUSEEVENTF_LEFTDOWN)?;
                if let Err(error) = self.claim_left_down() {
                    if Self::send_button(MOUSEEVENTF_LEFTUP).is_ok() {
                        let _ = self.clear_left_down();
                    }
                    return Err(error);
                }
            }
            ComputerAction::LeftMouseUp => self.post_left_up()?,
            ComputerAction::MouseMove { x, y } => {
                self.move_mouse(self.map_point(ScreenPoint { x: *x, y: *y })?)?;
            }
            ComputerAction::LeftClickDrag { from, to } => {
                let from = self.map_point(*from)?;
                let to = self.map_point(*to)?;
                self.move_mouse(from)?;
                cancel.check()?;
                Self::send_button(MOUSEEVENTF_LEFTDOWN)?;
                if let Err(error) = self.claim_left_down() {
                    if Self::send_button(MOUSEEVENTF_LEFTUP).is_ok() {
                        let _ = self.clear_left_down();
                    }
                    return Err(error);
                }
                let result = cancel
                    .check()
                    .and_then(|()| self.move_mouse(to))
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
            } => self.scroll(
                self.map_point(ScreenPoint { x: *x, y: *y })?,
                *direction,
                *amount,
                cancel,
            )?,
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
impl ComputerBackend for WindowsComputerBackend {
    async fn execute(
        &self,
        action: &ComputerAction,
        cancel: &ComputerCancelToken,
    ) -> ComputerResult<ComputerOutput> {
        cancel.check()?;
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
        let screen = state.pending_screen.ok_or_else(|| ComputerError::Backend {
            message: "CU-1 viewport arrived without a matching Windows capture".into(),
        })?;
        state.viewport = Some(Viewport {
            screen,
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

fn ensure_dpi_awareness() {
    DPI_AWARENESS.call_once(|| {
        // SAFETY: the pseudo-handle is a process-lifetime Windows constant.
        // Failure is intentionally best-effort: it normally means a manifest
        // or earlier process initialization already fixed the DPI context.
        let _ =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    });
}

fn send_input(input: &INPUT, context: &str) -> ComputerResult<()> {
    send_inputs(std::slice::from_ref(input), context)
}

fn send_inputs(inputs: &[INPUT], context: &str) -> ComputerResult<()> {
    let input_count = u32::try_from(inputs.len()).map_err(|_| ComputerError::Backend {
        message: "Windows input batch exceeds the SendInput count range".into(),
    })?;
    let input_size = i32::try_from(size_of::<INPUT>()).map_err(|_| ComputerError::Backend {
        message: "Windows INPUT structure size exceeds the SendInput range".into(),
    })?;
    // SAFETY: clear the thread's error slot so the documented UIPI case, which
    // may return zero without setting last-error, cannot expose stale detail.
    // The initialized INPUT slice is borrowed synchronously and cbSize matches
    // the windows-sys 0.61.2 ABI structure.
    let inserted = unsafe {
        SetLastError(0);
        SendInput(input_count, inputs.as_ptr(), input_size)
    };
    if inserted == input_count {
        Ok(())
    } else {
        Err(last_windows_unavailable(&format!(
            "{context} (inserted {inserted} of {input_count} events)"
        )))
    }
}

fn last_windows_unavailable(context: &str) -> ComputerError {
    let error = std::io::Error::last_os_error();
    let detail = match error.raw_os_error() {
        Some(0) | None => "Windows supplied no extended error (UIPI may do this)".to_owned(),
        Some(code) => format!("{error} (OS error {code})"),
    };
    no_interactive_desktop(format!("{context}: {detail}"))
}

fn no_interactive_desktop(message: String) -> ComputerError {
    ComputerError::Unavailable {
        platform: "windows".into(),
        message: format!(
            "{message}; verify Haider is running in an interactive Windows desktop session (not session 0). If the foreground app is elevated, run Haider elevated too because UIPI blocks lower-integrity input"
        ),
    }
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
    let relative_x =
        u64::from(point.x) * u64::from(viewport.screen.width) / u64::from(viewport.image_width);
    let relative_y =
        u64::from(point.y) * u64::from(viewport.screen.height) / u64::from(viewport.image_height);
    let x = i64::from(viewport.screen.left)
        .checked_add(
            i64::try_from(relative_x).map_err(|_| ComputerError::Backend {
                message: "mapped Windows x coordinate conversion failed".into(),
            })?,
        )
        .and_then(|value| i32::try_from(value).ok())
        .ok_or_else(|| ComputerError::InvalidAction {
            message: "mapped Windows x coordinate exceeds the virtual-screen range".into(),
        })?;
    let y = i64::from(viewport.screen.top)
        .checked_add(
            i64::try_from(relative_y).map_err(|_| ComputerError::Backend {
                message: "mapped Windows y coordinate conversion failed".into(),
            })?,
        )
        .and_then(|value| i32::try_from(value).ok())
        .ok_or_else(|| ComputerError::InvalidAction {
            message: "mapped Windows y coordinate exceeds the virtual-screen range".into(),
        })?;
    Ok(NativePoint { x, y })
}

fn normalize_absolute(screen: VirtualScreen, point: NativePoint) -> ComputerResult<(i32, i32)> {
    let relative_x = i64::from(point.x) - i64::from(screen.left);
    let relative_y = i64::from(point.y) - i64::from(screen.top);
    if relative_x < 0
        || relative_y < 0
        || relative_x >= i64::from(screen.width)
        || relative_y >= i64::from(screen.height)
    {
        return Err(ComputerError::InvalidAction {
            message: "mapped Windows coordinate is outside the captured virtual screen".into(),
        });
    }
    Ok((
        normalize_axis(relative_x as u32, screen.width),
        normalize_axis(relative_y as u32, screen.height),
    ))
}

fn normalize_axis(offset: u32, extent: u32) -> i32 {
    if extent <= 1 {
        0
    } else {
        (u64::from(offset) * ABSOLUTE_MAX / u64::from(extent - 1)) as i32
    }
}

fn modifier_key(name: &str) -> Option<KeySpec> {
    Some(match name.to_ascii_lowercase().as_str() {
        "cmd" | "command" | "meta" | "super" => KeySpec {
            virtual_key: VK_LWIN,
            extended: true,
        },
        "shift" => KeySpec {
            virtual_key: VK_SHIFT,
            extended: false,
        },
        "ctrl" | "control" => KeySpec {
            virtual_key: VK_CONTROL,
            extended: false,
        },
        "alt" | "option" => KeySpec {
            virtual_key: VK_MENU,
            extended: false,
        },
        _ => return None,
    })
}

fn key_binding(key: &str) -> ComputerResult<(KeySpec, Vec<KeySpec>)> {
    let layout_key = match key.to_ascii_lowercase().as_str() {
        "plus" => "+",
        "minus" => "-",
        _ => key,
    };
    if layout_key.chars().count() == 1 {
        let scalar = layout_key
            .chars()
            .next()
            .ok_or_else(|| ComputerError::InvalidAction {
                message: "keyboard shortcut is empty".into(),
            })?;
        let mut encoded = [0_u16; 2];
        let units = scalar.encode_utf16(&mut encoded);
        if units.len() != 1 {
            return Err(ComputerError::InvalidAction {
                message: format!(
                    "unsupported Windows shortcut key `{key}`; use the type action for supplementary Unicode text"
                ),
            });
        }
        // SAFETY: VkKeyScanW accepts one UTF-16 code unit by value and reads
        // the active keyboard layout without transferring ownership.
        let mapped = unsafe { VkKeyScanW(units[0]) };
        if mapped == -1 {
            return Err(ComputerError::InvalidAction {
                message: format!(
                    "the active Windows keyboard layout has no virtual key for `{key}`"
                ),
            });
        }
        let packed = mapped as u16;
        let shift_state = (packed >> 8) as u8;
        if shift_state & !0x07 != 0 {
            return Err(ComputerError::InvalidAction {
                message: format!(
                    "the active Windows keyboard layout requires unsupported modifier state 0x{shift_state:02x} for `{key}`"
                ),
            });
        }
        let mut modifiers = Vec::new();
        for (mask, virtual_key) in [(0x01, VK_SHIFT), (0x02, VK_CONTROL), (0x04, VK_MENU)] {
            if shift_state & mask != 0 {
                modifiers.push(KeySpec {
                    virtual_key,
                    extended: false,
                });
            }
        }
        return Ok((
            KeySpec {
                virtual_key: packed & 0xff,
                extended: false,
            },
            modifiers,
        ));
    }
    let spec = named_key(key).ok_or_else(|| ComputerError::InvalidAction {
        message: format!("unsupported Windows key `{key}`"),
    })?;
    Ok((spec, Vec::new()))
}

fn named_key(key: &str) -> Option<KeySpec> {
    let lower = key.to_ascii_lowercase();
    let (virtual_key, extended) = match lower.as_str() {
        "backspace" => (VK_BACK, false),
        "tab" => (VK_TAB, false),
        "return" | "enter" => (VK_RETURN, false),
        "escape" | "esc" => (VK_ESCAPE, false),
        "home" => (VK_HOME, true),
        "left" => (VK_LEFT, true),
        "up" => (VK_UP, true),
        "right" => (VK_RIGHT, true),
        "down" => (VK_DOWN, true),
        "pageup" | "page_up" => (VK_PRIOR, true),
        "pagedown" | "page_down" => (VK_NEXT, true),
        "end" => (VK_END, true),
        "delete" => (VK_DELETE, true),
        "space" => (VK_SPACE, false),
        "f1" => (VK_F1, false),
        "f2" => (VK_F1 + 1, false),
        "f3" => (VK_F1 + 2, false),
        "f4" => (VK_F1 + 3, false),
        "f5" => (VK_F1 + 4, false),
        "f6" => (VK_F1 + 5, false),
        "f7" => (VK_F1 + 6, false),
        "f8" => (VK_F1 + 7, false),
        "f9" => (VK_F1 + 8, false),
        "f10" => (VK_F1 + 9, false),
        "f11" => (VK_F1 + 10, false),
        "f12" => (VK_F1 + 11, false),
        _ => return None,
    };
    Some(KeySpec {
        virtual_key,
        extended,
    })
}

fn push_unique(keys: &mut Vec<KeySpec>, key: KeySpec) {
    if !keys.contains(&key) {
        keys.push(key);
    }
}

fn release_keys(keys: &[KeySpec]) {
    for key in keys.iter().rev().copied() {
        let _ = WindowsComputerBackend::post_key(key, true);
    }
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

struct ScreenDc(HDC);

impl Drop for ScreenDc {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: this exactly balances GetDC(NULL).
            let _ = unsafe { ReleaseDC(ptr::null_mut(), self.0) };
        }
    }
}

struct MemoryDc(HDC);

impl Drop for MemoryDc {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: this exactly balances CreateCompatibleDC.
            let _ = unsafe { DeleteDC(self.0) };
        }
    }
}

struct OwnedBitmap(HBITMAP);

impl Drop for OwnedBitmap {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: this exactly balances CreateCompatibleBitmap after its
            // selection guard has restored the memory DC's original object.
            let _ = unsafe { DeleteObject(self.0) };
        }
    }
}

struct SelectedBitmap<'a> {
    dc: &'a MemoryDc,
    _bitmap: &'a OwnedBitmap,
    old: HGDIOBJ,
}

impl<'a> SelectedBitmap<'a> {
    fn new(dc: &'a MemoryDc, bitmap: &'a OwnedBitmap) -> ComputerResult<Self> {
        // SAFETY: both handles are live, and the returned old object remains
        // owned by the DC. It is restored before either borrowed handle drops.
        let old = unsafe { SelectObject(dc.0, bitmap.0) };
        if old.is_null() {
            Err(last_windows_unavailable(
                "could not select the Windows screenshot bitmap",
            ))
        } else {
            Ok(Self {
                dc,
                _bitmap: bitmap,
                old,
            })
        }
    }

    fn restore(mut self) -> ComputerResult<()> {
        let old = self.old;
        // SAFETY: old came from SelectObject on this live DC.
        if unsafe { SelectObject(self.dc.0, old) }.is_null() {
            return Err(last_windows_unavailable(
                "could not restore the Windows screenshot device context",
            ));
        }
        self.old = ptr::null_mut();
        Ok(())
    }
}

impl Drop for SelectedBitmap<'_> {
    fn drop(&mut self) {
        if !self.old.is_null() {
            // SAFETY: best-effort restoration during error unwinding; if it
            // fails, MemoryDc drops before OwnedBitmap and releases selection.
            let _ = unsafe { SelectObject(self.dc.0, self.old) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivered_cu1_pixels_map_through_negative_virtual_origin() {
        let viewport = Viewport {
            screen: VirtualScreen {
                left: -1_920,
                top: -200,
                width: 5_120,
                height: 1_800,
            },
            image_width: 2_560,
            image_height: 900,
        };
        let point = match map_delivered_pixel(viewport, ScreenPoint { x: 960, y: 450 }) {
            Ok(point) => point,
            Err(error) => panic!("point must map: {error}"),
        };
        assert_eq!(point, NativePoint { x: 0, y: 700 });
        assert!(map_delivered_pixel(viewport, ScreenPoint { x: 2_560, y: 0 }).is_err());
    }

    #[test]
    fn virtual_screen_pixels_normalize_to_sendinput_absolute_space() {
        let screen = VirtualScreen {
            left: -1_920,
            top: -200,
            width: 5_120,
            height: 1_800,
        };
        assert_eq!(
            normalize_absolute(screen, NativePoint { x: -1_920, y: -200 }),
            Ok((0, 0))
        );
        assert_eq!(
            normalize_absolute(screen, NativePoint { x: 3_199, y: 1_599 }),
            Ok((65_535, 65_535))
        );
    }

    #[test]
    fn named_keys_and_modifiers_match_windows_virtual_keys() {
        assert_eq!(
            named_key("enter").map(|key| key.virtual_key),
            Some(VK_RETURN)
        );
        assert_eq!(
            named_key("F12").map(|key| key.virtual_key),
            Some(VK_F1 + 11)
        );
        assert!(named_key("left").is_some_and(|key| key.extended));
        assert_eq!(
            modifier_key("command").map(|key| key.virtual_key),
            Some(VK_LWIN)
        );
        assert!(named_key("not-a-key").is_none());
    }

    #[test]
    fn supplementary_unicode_is_one_canonical_sendinput_batch() {
        let inputs = WindowsComputerBackend::unicode_scalar_inputs('\u{1f642}');
        assert_eq!(inputs.len(), 4);
        let events = inputs
            .iter()
            .map(|input| {
                // SAFETY: unicode_scalar_inputs initializes the keyboard arm
                // of every INPUT union value.
                let keyboard = unsafe { input.Anonymous.ki };
                (keyboard.wVk, keyboard.wScan, keyboard.dwFlags)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            events,
            vec![
                (0, 0xd83d, KEYEVENTF_UNICODE),
                (0, 0xde42, KEYEVENTF_UNICODE),
                (0, 0xd83d, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP),
                (0, 0xde42, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP),
            ]
        );
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

    /// Real GDI exercise for manual Windows desktop validation only. GitHub
    /// Windows runners are headless; CI compiles/clippies this backend and uses
    /// the deterministic fake ComputerBackend for runtime semantics.
    #[tokio::test]
    #[ignore = "requires an interactive Windows desktop"]
    async fn manual_real_hardware_screenshot() {
        let backend = WindowsComputerBackend::new();
        let output = match backend
            .execute(&ComputerAction::Screenshot, &ComputerCancelToken::new())
            .await
        {
            Ok(output) => output,
            Err(error) => panic!("manual Windows screenshot failed: {error}"),
        };
        assert!(matches!(output, ComputerOutput::ScreenshotPng(bytes) if !bytes.is_empty()));
    }
}
