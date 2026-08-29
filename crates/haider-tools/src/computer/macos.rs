//! macOS CoreGraphics backend.
//!
//! `CGEvent` consumes global Quartz points while screenshots are delivered as
//! pixels. The retained viewport maps the exact post-CU-1 image dimensions to
//! `CGDisplayBounds`, so Retina backing scale and CU-1 downscaling are both
//! accounted for without a hard-coded scale factor.

use super::{
    ComputerBackend, ComputerCancelToken, ComputerError, ComputerInspection,
    ComputerInspectionBounds, ComputerOutput, ComputerPermissionPoll, ComputerResult,
};
use async_trait::async_trait;
use haider_protocol::computer::{ComputerAction, ScreenPoint, ScrollDirection};
use haider_protocol::permission::SystemPermission;
use image::{DynamicImage, ImageFormat, RgbaImage};
use std::ffi::{CStr, c_char, c_void};
use std::io::Cursor;
use std::process::{Command, Stdio};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

const SCREEN_RECORDING_PANE: &str = "System Settings > Privacy & Security > Screen Recording";
const SCREEN_RECORDING_URL: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture";
const ACCESSIBILITY_PANE: &str = "System Settings > Privacy & Security > Accessibility";
const ACCESSIBILITY_URL: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";

pub(crate) fn open_system_permission_settings(permission: SystemPermission) -> ComputerResult<()> {
    let url = match permission {
        SystemPermission::ScreenRecording => SCREEN_RECORDING_URL,
        SystemPermission::Accessibility => ACCESSIBILITY_URL,
    };
    Command::new("/usr/bin/open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| ComputerError::Backend {
            message: format!("could not open macOS System Settings: {error}"),
        })
}

const CG_EVENT_TAP_HID: u32 = 0;
const CG_MOUSE_LEFT: u32 = 0;
const CG_MOUSE_RIGHT: u32 = 1;
const CG_MOUSE_CENTER: u32 = 2;
const CG_EVENT_LEFT_DOWN: u32 = 1;
const CG_EVENT_LEFT_UP: u32 = 2;
const CG_EVENT_RIGHT_DOWN: u32 = 3;
const CG_EVENT_RIGHT_UP: u32 = 4;
const CG_EVENT_MOUSE_MOVED: u32 = 5;
const CG_EVENT_LEFT_DRAGGED: u32 = 6;
const CG_EVENT_OTHER_DOWN: u32 = 25;
const CG_EVENT_OTHER_UP: u32 = 26;
const CG_SCROLL_EVENT_UNIT_LINE: u32 = 1;
const CG_MOUSE_EVENT_CLICK_STATE: u32 = 1;
const CG_FLAG_SHIFT: u64 = 0x0002_0000;
const CG_FLAG_CONTROL: u64 = 0x0004_0000;
const CG_FLAG_OPTION: u64 = 0x0008_0000;
const CG_FLAG_COMMAND: u64 = 0x0010_0000;
const CG_IMAGE_ALPHA_PREMULTIPLIED_LAST: u32 = 1;
const CG_BITMAP_BYTE_ORDER_32_BIG: u32 = 4 << 12;
const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const AX_ERROR_SUCCESS: i32 = 0;
const AX_ERROR_ATTRIBUTE_UNSUPPORTED: i32 = -25_205;
const AX_ERROR_NO_VALUE: i32 = -25_212;
const AX_VALUE_CGRECT: u32 = 3;

static NEXT_INPUT_OWNER: AtomicU64 = AtomicU64::new(1);
static INPUT_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static HELD_LEFT_OWNER: Mutex<Option<u64>> = Mutex::new(None);

type CFTypeRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFStringRef = *const c_void;
type AXUIElementRef = *const c_void;
type AXValueRef = *const c_void;
type CGImageRef = *mut c_void;
type CGColorSpaceRef = *mut c_void;
type CGContextRef = *mut c_void;
type CGEventRef = *mut c_void;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

// Keep these ABI declarations aligned with the owner's working
// rust-diffforge `snipping.rs` CoreGraphics shapes (whose macOS dependency set
// is objc2-core-graphics 0.3.2). The TCC functions are raw externs there too;
// this is a separate backend, not a fork or import of the snipping module.
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
    fn CGMainDisplayID() -> u32;
    fn CGDisplayBounds(display: u32) -> CGRect;
    fn CGDisplayCreateImage(display: u32) -> CGImageRef;
    fn CGImageGetWidth(image: CGImageRef) -> usize;
    fn CGImageGetHeight(image: CGImageRef) -> usize;
    fn CGImageRelease(image: CGImageRef);
    fn CGColorSpaceCreateDeviceRGB() -> CGColorSpaceRef;
    fn CGColorSpaceRelease(color_space: CGColorSpaceRef);
    fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        color_space: CGColorSpaceRef,
        bitmap_info: u32,
    ) -> CGContextRef;
    fn CGContextTranslateCTM(context: CGContextRef, tx: f64, ty: f64);
    fn CGContextScaleCTM(context: CGContextRef, sx: f64, sy: f64);
    fn CGContextDrawImage(context: CGContextRef, rect: CGRect, image: CGImageRef);
    fn CGContextRelease(context: CGContextRef);
    fn CGEventCreate(source: *const c_void) -> CGEventRef;
    fn CGEventCreateMouseEvent(
        source: *const c_void,
        event_type: u32,
        position: CGPoint,
        button: u32,
    ) -> CGEventRef;
    fn CGEventCreateKeyboardEvent(
        source: *const c_void,
        virtual_key: u16,
        key_down: bool,
    ) -> CGEventRef;
    fn CGEventCreateScrollWheelEvent(
        source: *const c_void,
        units: u32,
        wheel_count: u32,
        ...
    ) -> CGEventRef;
    fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
    fn CGEventSetLocation(event: CGEventRef, location: CGPoint);
    fn CGEventSetFlags(event: CGEventRef, flags: u64);
    fn CGEventSetIntegerValueField(event: CGEventRef, field: u32, value: i64);
    fn CGEventKeyboardSetUnicodeString(event: CGEventRef, length: usize, text: *const u16);
    fn CGEventPost(tap: u32, event: CGEventRef);
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> u8;
    fn AXUIElementCreateSystemWide() -> AXUIElementRef;
    fn AXUIElementCopyElementAtPosition(
        application: AXUIElementRef,
        x: f32,
        y: f32,
        element: *mut AXUIElementRef,
    ) -> i32;
    fn AXUIElementCopyAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> i32;
    fn AXValueGetTypeID() -> usize;
    fn AXValueGetValue(value: AXValueRef, value_type: u32, value_ptr: *mut c_void) -> u8;
    static kAXTrustedCheckOptionPrompt: CFTypeRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFBooleanTrue: CFTypeRef;
    static kCFBooleanFalse: CFTypeRef;
    fn CFDictionaryCreate(
        allocator: *const c_void,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        count: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFDictionaryRef;
    fn CFGetTypeID(value: CFTypeRef) -> usize;
    fn CFCopyDescription(value: CFTypeRef) -> CFStringRef;
    fn CFStringCreateWithCString(
        allocator: *const c_void,
        c_string: *const c_char,
        encoding: u32,
    ) -> CFStringRef;
    fn CFStringGetTypeID() -> usize;
    fn CFStringGetLength(string: CFStringRef) -> isize;
    fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
    fn CFStringGetCString(
        string: CFStringRef,
        buffer: *mut c_char,
        buffer_size: isize,
        encoding: u32,
    ) -> u8;
    fn CFRelease(value: CFTypeRef);
}

#[derive(Debug, Clone, Copy)]
struct Viewport {
    display_bounds: CGRect,
    image_width: u32,
    image_height: u32,
}

#[derive(Debug, Default)]
struct BackendState {
    pending_display_bounds: Option<CGRect>,
    viewport: Option<Viewport>,
    left_button_down: bool,
}

#[derive(Debug)]
pub(crate) struct MacOsComputerBackend {
    input_owner: u64,
    state: Mutex<BackendState>,
    screen_capture_confirmed: AtomicBool,
    screen_capture_requested: AtomicBool,
    accessibility_requested: AtomicBool,
}

impl MacOsComputerBackend {
    pub(crate) fn new() -> Self {
        Self {
            input_owner: NEXT_INPUT_OWNER.fetch_add(1, Ordering::Relaxed),
            state: Mutex::new(BackendState::default()),
            screen_capture_confirmed: AtomicBool::new(false),
            screen_capture_requested: AtomicBool::new(false),
            accessibility_requested: AtomicBool::new(false),
        }
    }

    fn lock_state(&self) -> ComputerResult<MutexGuard<'_, BackendState>> {
        self.state.lock().map_err(|_| ComputerError::Backend {
            message: "macOS computer viewport lock is poisoned".into(),
        })
    }

    fn preflight_screen_recording(&self) -> ComputerResult<()> {
        if self.screen_capture_confirmed.load(Ordering::Acquire) {
            return Ok(());
        }
        // SAFETY: these are parameterless CoreGraphics TCC APIs. They return
        // immediately; no pointer ownership crosses the call.
        if unsafe { CGPreflightScreenCaptureAccess() } {
            self.screen_capture_confirmed.store(true, Ordering::Release);
            return Ok(());
        }
        if !self.screen_capture_requested.swap(true, Ordering::AcqRel)
            // SAFETY: same parameterless API; macOS owns the system prompt.
            && unsafe { CGRequestScreenCaptureAccess() }
        {
            self.screen_capture_confirmed.store(true, Ordering::Release);
            return Ok(());
        }
        Err(ComputerError::PermissionRequired {
            permission: SystemPermission::ScreenRecording,
            settings_pane: SCREEN_RECORDING_PANE.into(),
            settings_url: SCREEN_RECORDING_URL.into(),
            restart_required: false,
            message: format!(
                "macOS Screen Recording permission is waiting in {SCREEN_RECORDING_PANE}"
            ),
        })
    }

    fn accessibility_options(prompt: bool) -> ComputerResult<CFDictionaryRef> {
        // SAFETY: the imported CF constants have process lifetime. The
        // returned retained dictionary is released by the caller.
        let key = unsafe { kAXTrustedCheckOptionPrompt };
        let value = if prompt {
            // SAFETY: this imported singleton has process lifetime and is only
            // borrowed while constructing the dictionary.
            unsafe { kCFBooleanTrue }
        } else {
            // SAFETY: this imported singleton has process lifetime and is only
            // borrowed while constructing the dictionary.
            unsafe { kCFBooleanFalse }
        };
        // SAFETY: one valid key/value pointer is provided and null callbacks
        // request the standard CF object semantics used by the owner reference.
        let options =
            unsafe { CFDictionaryCreate(ptr::null(), &key, &value, 1, ptr::null(), ptr::null()) };
        if options.is_null() {
            Err(ComputerError::Backend {
                message: "could not construct macOS Accessibility preflight options".into(),
            })
        } else {
            Ok(options)
        }
    }

    fn accessibility_trusted(prompt: bool) -> ComputerResult<bool> {
        let options = Self::accessibility_options(prompt)?;
        // SAFETY: `options` is a live CFDictionary created above.
        let trusted = unsafe { AXIsProcessTrustedWithOptions(options) != 0 };
        // SAFETY: balance the create rule exactly once.
        unsafe { CFRelease(options) };
        Ok(trusted)
    }

    fn preflight_accessibility(&self) -> ComputerResult<()> {
        if Self::accessibility_trusted(false)? {
            return Ok(());
        }
        if !self.accessibility_requested.swap(true, Ordering::AcqRel)
            && Self::accessibility_trusted(true)?
        {
            return Ok(());
        }
        Err(ComputerError::PermissionRequired {
            permission: SystemPermission::Accessibility,
            settings_pane: ACCESSIBILITY_PANE.into(),
            settings_url: ACCESSIBILITY_URL.into(),
            restart_required: false,
            message: format!("macOS Accessibility permission is waiting in {ACCESSIBILITY_PANE}"),
        })
    }

    fn capture_png_blocking(cancel: &ComputerCancelToken) -> ComputerResult<(Vec<u8>, CGRect)> {
        cancel.check()?;
        // SAFETY: display id/bounds are value APIs. Create returns a retained
        // CGImage which is released on every later path.
        let display = unsafe { CGMainDisplayID() };
        let bounds = unsafe { CGDisplayBounds(display) };
        let image = unsafe { CGDisplayCreateImage(display) };
        if image.is_null() {
            return Err(ComputerError::PermissionRequired {
                permission: SystemPermission::ScreenRecording,
                settings_pane: SCREEN_RECORDING_PANE.into(),
                settings_url: SCREEN_RECORDING_URL.into(),
                restart_required: true,
                message: "macOS granted Screen Recording, but this haiderd process cannot capture until it restarts".into(),
            });
        }
        // SAFETY: `image` remains live until the guarded release below.
        let (width, height) = unsafe { (CGImageGetWidth(image), CGImageGetHeight(image)) };
        let Some(byte_len) = width
            .checked_mul(height)
            .and_then(|pixels| pixels.checked_mul(4))
        else {
            // SAFETY: `image` is the retained non-null create result and no
            // other path has released it.
            unsafe { CGImageRelease(image) };
            return Err(ComputerError::Backend {
                message: "macOS screen dimensions overflow the capture buffer".into(),
            });
        };
        if width == 0 || height == 0 || width > u32::MAX as usize || height > u32::MAX as usize {
            // SAFETY: `image` is still the retained non-null create result and
            // this failing branch releases it exactly once.
            unsafe { CGImageRelease(image) };
            return Err(ComputerError::Backend {
                message: "macOS screen capture returned invalid dimensions".into(),
            });
        }
        let mut rgba = vec![0_u8; byte_len];
        // SAFETY: create returns a retained color space.
        let color_space = unsafe { CGColorSpaceCreateDeviceRGB() };
        if color_space.is_null() {
            // SAFETY: `image` remains the retained create result; color-space
            // creation failed and produced no object requiring release.
            unsafe { CGImageRelease(image) };
            return Err(ComputerError::Backend {
                message: "could not create an RGB color space for screen capture".into(),
            });
        }
        // SAFETY: the context borrows `rgba` for this synchronous scope; the
        // checked size and stride exactly cover width*height RGBA pixels.
        let context = unsafe {
            CGBitmapContextCreate(
                rgba.as_mut_ptr().cast(),
                width,
                height,
                8,
                width * 4,
                color_space,
                CG_IMAGE_ALPHA_PREMULTIPLIED_LAST | CG_BITMAP_BYTE_ORDER_32_BIG,
            )
        };
        if context.is_null() {
            // SAFETY: both objects are live retained create results, and this
            // branch releases each once after context creation failed.
            unsafe {
                CGColorSpaceRelease(color_space);
                CGImageRelease(image);
            }
            return Err(ComputerError::Backend {
                message: "could not create an RGBA screen capture context".into(),
            });
        }
        // SAFETY: all retained objects are live and the target rect is within
        // the allocated bitmap. The transform yields top-left image rows.
        unsafe {
            CGContextTranslateCTM(context, 0.0, height as f64);
            CGContextScaleCTM(context, 1.0, -1.0);
            CGContextDrawImage(
                context,
                CGRect {
                    origin: CGPoint { x: 0.0, y: 0.0 },
                    size: CGSize {
                        width: width as f64,
                        height: height as f64,
                    },
                },
                image,
            );
            CGContextRelease(context);
            CGColorSpaceRelease(color_space);
            CGImageRelease(image);
        }
        cancel.check()?;
        let buffer = RgbaImage::from_raw(width as u32, height as u32, rgba).ok_or_else(|| {
            ComputerError::Backend {
                message: "macOS RGBA capture buffer has inconsistent dimensions".into(),
            }
        })?;
        let mut encoded = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(buffer)
            .write_to(&mut encoded, ImageFormat::Png)
            .map_err(|error| ComputerError::Backend {
                message: format!("could not encode macOS screenshot as PNG: {error}"),
            })?;
        cancel.check()?;
        Ok((encoded.into_inner(), bounds))
    }

    async fn capture_png(&self, cancel: &ComputerCancelToken) -> ComputerResult<Vec<u8>> {
        self.preflight_screen_recording()?;
        let worker_cancel = cancel.clone();
        let mut capture =
            tokio::task::spawn_blocking(move || Self::capture_png_blocking(&worker_cancel));
        let (png, bounds) = tokio::select! {
            biased;
            () = wait_for_cancel(cancel) => {
                capture.abort();
                return Err(ComputerError::Cancelled);
            }
            result = &mut capture => result.map_err(|error| ComputerError::Backend {
                message: format!("macOS screenshot worker failed: {error}"),
            })??,
        };
        cancel.check()?;
        let mut state = self.lock_state()?;
        state.pending_display_bounds = Some(bounds);
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

    fn map_point(&self, point: ScreenPoint) -> ComputerResult<CGPoint> {
        let viewport = self.viewport()?;
        map_delivered_pixel(viewport, point)
    }

    fn current_quartz_point() -> ComputerResult<CGPoint> {
        // SAFETY: CGEventCreate returns a retained snapshot event.
        let event = unsafe { CGEventCreate(ptr::null()) };
        if event.is_null() {
            return Err(ComputerError::Backend {
                message: "macOS could not read the cursor position".into(),
            });
        }
        // SAFETY: the event is live for this call and then released as CFType.
        let point = unsafe { CGEventGetLocation(event) };
        unsafe { CFRelease(event.cast_const()) };
        Ok(point)
    }

    fn model_cursor_position(&self) -> ComputerResult<(u32, u32)> {
        self.preflight_screen_recording()?;
        let viewport = self.viewport()?;
        let point = Self::current_quartz_point()?;
        let bounds = viewport.display_bounds;
        if point.x < bounds.origin.x
            || point.y < bounds.origin.y
            || point.x >= bounds.origin.x + bounds.size.width
            || point.y >= bounds.origin.y + bounds.size.height
        {
            return Err(ComputerError::InvalidAction {
                message: "cursor is outside the display captured by the latest computer screenshot"
                    .into(),
            });
        }
        let x = ((point.x - bounds.origin.x) * f64::from(viewport.image_width) / bounds.size.width)
            .floor() as u32;
        let y = ((point.y - bounds.origin.y) * f64::from(viewport.image_height)
            / bounds.size.height)
            .floor() as u32;
        Ok((x, y))
    }

    fn inspect(
        &self,
        x: u32,
        y: u32,
        cancel: &ComputerCancelToken,
    ) -> ComputerResult<ComputerInspection> {
        cancel.check()?;
        self.preflight_accessibility()?;
        let viewport = self.viewport()?;
        let point = map_delivered_pixel(viewport, ScreenPoint { x, y })?;

        // SAFETY: create returns a retained process-wide accessibility object.
        let system_wide = unsafe { AXUIElementCreateSystemWide() };
        if system_wide.is_null() {
            return Err(ComputerError::Backend {
                message: "macOS could not create the system-wide accessibility element".into(),
            });
        }
        let mut element: AXUIElementRef = ptr::null();
        // SAFETY: `system_wide` is live, the point is finite and mapped from a
        // validated screenshot pixel, and `element` is a writable out pointer.
        let ax_error = unsafe {
            AXUIElementCopyElementAtPosition(
                system_wide,
                point.x as f32,
                point.y as f32,
                &mut element,
            )
        };
        // SAFETY: balance the create rule for the system-wide object.
        unsafe { CFRelease(system_wide) };
        if ax_error != AX_ERROR_SUCCESS {
            return Err(if ax_error == AX_ERROR_NO_VALUE {
                ComputerError::InvalidAction {
                    message: format!(
                        "no accessibility element exists at screenshot pixel ({x}, {y})"
                    ),
                }
            } else {
                ComputerError::Backend {
                    message: format!(
                        "macOS accessibility hit-test failed at screenshot pixel ({x}, {y}) (AXError {ax_error})"
                    ),
                }
            });
        }
        if element.is_null() {
            return Err(ComputerError::Backend {
                message: "macOS accessibility hit-test returned no element".into(),
            });
        }

        let inspection = (|| {
            cancel.check()?;
            Ok(ComputerInspection {
                role: copy_ax_text_attribute(element, c"AXRole")?,
                label: copy_ax_text_attribute(element, c"AXDescription")?,
                title: copy_ax_text_attribute(element, c"AXTitle")?,
                bounds: copy_ax_bounds(element)?
                    .and_then(|bounds| map_accessibility_bounds(viewport, bounds)),
                value: copy_ax_text_attribute(element, c"AXValue")?,
            })
        })();
        // SAFETY: the successful copy-at-position call returned this retained
        // accessibility element exactly once.
        unsafe { CFRelease(element) };
        inspection
    }

    fn post_mouse(
        event_type: u32,
        point: CGPoint,
        button: u32,
        click_state: i64,
    ) -> ComputerResult<()> {
        // SAFETY: null source requests the system source; the returned event is
        // retained and is released after synchronous posting.
        let event = unsafe { CGEventCreateMouseEvent(ptr::null(), event_type, point, button) };
        if event.is_null() {
            return Err(ComputerError::Backend {
                message: "macOS could not create a mouse event".into(),
            });
        }
        if click_state > 0 {
            // SAFETY: `event` is the live non-null create result and the field
            // setter borrows it without retaining any caller pointer.
            unsafe { CGEventSetIntegerValueField(event, CG_MOUSE_EVENT_CLICK_STATE, click_state) };
        }
        // SAFETY: `event` remains live through the synchronous post and is
        // released exactly once immediately afterward.
        unsafe {
            CGEventPost(CG_EVENT_TAP_HID, event);
            CFRelease(event.cast_const());
        }
        Ok(())
    }

    fn held_left_owner() -> ComputerResult<Option<u64>> {
        HELD_LEFT_OWNER
            .lock()
            .map(|owner| *owner)
            .map_err(|_| ComputerError::Backend {
                message: "macOS computer input-owner lock is poisoned".into(),
            })
    }

    /// Records that a synthetic left-down crossed the OS boundary. The global
    /// owner is written first: if the local state lock is poisoned, other
    /// sessions still fail closed until emergency cleanup posts a matching up.
    fn claim_left_down(&self) -> ComputerResult<()> {
        {
            let mut owner = HELD_LEFT_OWNER.lock().map_err(|_| ComputerError::Backend {
                message: "macOS computer input-owner lock is poisoned".into(),
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

    /// Clears both views only after a left-up successfully crossed the OS
    /// boundary. The global owner is authoritative for cross-session safety.
    fn clear_left_down(&self) -> ComputerResult<()> {
        {
            let mut owner = HELD_LEFT_OWNER.lock().map_err(|_| ComputerError::Backend {
                message: "macOS computer input-owner lock is poisoned".into(),
            })?;
            if *owner == Some(self.input_owner) {
                *owner = None;
            }
        }
        self.lock_state()?.left_button_down = false;
        Ok(())
    }

    fn post_left_up(&self, point: CGPoint, click_state: i64) -> ComputerResult<()> {
        Self::post_mouse(CG_EVENT_LEFT_UP, point, CG_MOUSE_LEFT, click_state)?;
        self.clear_left_down()
    }

    fn click(
        &self,
        point: CGPoint,
        button: u32,
        down: u32,
        up: u32,
        click_state: i64,
        cancel: &ComputerCancelToken,
    ) -> ComputerResult<()> {
        cancel.check()?;
        Self::post_mouse(down, point, button, click_state)?;
        if button == CG_MOUSE_LEFT
            && let Err(error) = self.claim_left_down()
        {
            if Self::post_mouse(up, point, button, click_state).is_ok() {
                let _ = self.clear_left_down();
            }
            return Err(error);
        }
        let release = || {
            if button == CG_MOUSE_LEFT {
                self.post_left_up(point, click_state)
            } else {
                Self::post_mouse(up, point, button, click_state)
            }
        };
        let result = cancel.check().and_then(|()| release());
        if result.is_ok() {
            return result;
        }
        // A down event crossed the OS boundary. Always make a second release
        // attempt on every later error; retain both owner records if that also
        // fails so dispatcher emergency-stop can try again.
        let _ = release();
        result
    }

    fn post_text_chunk(chunk: &[u16]) -> ComputerResult<()> {
        for key_down in [true, false] {
            // SAFETY: create returns a retained event; Unicode data is
            // borrowed only for the setter call and event post.
            let event = unsafe { CGEventCreateKeyboardEvent(ptr::null(), 0, key_down) };
            if event.is_null() {
                return Err(ComputerError::Backend {
                    message: "macOS could not create a text keyboard event".into(),
                });
            }
            // SAFETY: `event` is live; `chunk` remains allocated for the
            // synchronous setter, then the event is posted and released once.
            unsafe {
                CGEventKeyboardSetUnicodeString(event, chunk.len(), chunk.as_ptr());
                CGEventPost(CG_EVENT_TAP_HID, event);
                CFRelease(event.cast_const());
            }
        }
        Ok(())
    }

    async fn type_text(&self, text: &str, cancel: &ComputerCancelToken) -> ComputerResult<()> {
        let mut chunk = Vec::with_capacity(20);
        for scalar in text.chars() {
            let mut encoded = [0_u16; 2];
            let units = scalar.encode_utf16(&mut encoded);
            if !chunk.is_empty() && chunk.len() + units.len() > 20 {
                cancel.check()?;
                Self::post_text_chunk(&chunk)?;
                chunk.clear();
                // Give core's ESC branch a scheduling point between bounded
                // Unicode-safe event batches.
                tokio::task::yield_now().await;
            }
            chunk.extend_from_slice(units);
        }
        if !chunk.is_empty() {
            cancel.check()?;
            Self::post_text_chunk(&chunk)?;
        }
        cancel.check()?;
        Ok(())
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
        let mut flags = 0_u64;
        for modifier in parts {
            flags |= match modifier.to_ascii_lowercase().as_str() {
                "cmd" | "command" | "meta" => CG_FLAG_COMMAND,
                "shift" => CG_FLAG_SHIFT,
                "ctrl" | "control" => CG_FLAG_CONTROL,
                "alt" | "option" => CG_FLAG_OPTION,
                unknown => {
                    return Err(ComputerError::InvalidAction {
                        message: format!("unsupported macOS key modifier `{unknown}`"),
                    });
                }
            };
        }
        let key_code = virtual_key_code(last).ok_or_else(|| ComputerError::InvalidAction {
            message: format!("unsupported macOS key `{last}`"),
        })?;
        let post = |key_down| -> ComputerResult<()> {
            // SAFETY: null requests the system event source; key code and state
            // are value arguments and success returns a retained event.
            let event = unsafe { CGEventCreateKeyboardEvent(ptr::null(), key_code, key_down) };
            if event.is_null() {
                return Err(ComputerError::Backend {
                    message: "macOS could not create a keyboard shortcut event".into(),
                });
            }
            // SAFETY: `event` is the live create result, flags are a value,
            // and the synchronous post is followed by its single release.
            unsafe {
                CGEventSetFlags(event, flags);
                CGEventPost(CG_EVENT_TAP_HID, event);
                CFRelease(event.cast_const());
            }
            Ok(())
        };
        cancel.check()?;
        post(true)?;
        if let Err(error) = cancel.check() {
            let _ = post(false);
            return Err(error);
        }
        post(false)?;
        Ok(())
    }

    fn scroll(
        &self,
        point: CGPoint,
        direction: ScrollDirection,
        amount: u32,
        cancel: &ComputerCancelToken,
    ) -> ComputerResult<()> {
        let amount = i32::try_from(amount).map_err(|_| ComputerError::InvalidAction {
            message: "computer scroll amount exceeds the macOS event range".into(),
        })?;
        cancel.check()?;
        Self::post_mouse(CG_EVENT_MOUSE_MOVED, point, CG_MOUSE_LEFT, 0)?;
        let (horizontal, delta) = match direction {
            ScrollDirection::Up => (false, amount),
            ScrollDirection::Down => (false, -amount),
            ScrollDirection::Left => (true, amount),
            ScrollDirection::Right => (true, -amount),
        };
        // SAFETY: the variadic constructor receives exactly the declared i32
        // axis count. Horizontal scrolling supplies zero vertical delta and
        // the requested second-axis delta.
        let event = if horizontal {
            // SAFETY: the variadic call receives exactly two i32 axis values,
            // matching its declared count, and null requests a system source.
            unsafe {
                CGEventCreateScrollWheelEvent(
                    ptr::null(),
                    CG_SCROLL_EVENT_UNIT_LINE,
                    2,
                    0_i32,
                    delta,
                )
            }
        } else {
            // SAFETY: the variadic call receives exactly one i32 axis value,
            // matching its declared count, and null requests a system source.
            unsafe {
                CGEventCreateScrollWheelEvent(ptr::null(), CG_SCROLL_EVENT_UNIT_LINE, 1, delta)
            }
        };
        if event.is_null() {
            return Err(ComputerError::Backend {
                message: "macOS could not create a scroll event".into(),
            });
        }
        // SAFETY: `event` is the live retained create result; location/post
        // borrow it synchronously and CFRelease balances creation exactly once.
        unsafe {
            CGEventSetLocation(event, point);
            CGEventPost(CG_EVENT_TAP_HID, event);
            CFRelease(event.cast_const());
        }
        Ok(())
    }

    async fn control(
        &self,
        action: &ComputerAction,
        cancel: &ComputerCancelToken,
    ) -> ComputerResult<ComputerOutput> {
        self.preflight_accessibility()?;
        // Quartz input is process-global. Serialize complete actions and
        // reserve a persistent left-button hold for its originating backend,
        // while keeping screenshot/viewport state dispatcher-local.
        let _input_gate = INPUT_GATE.lock().await;
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
        // A successful delivered screenshot is the only authoritative model
        // coordinate space, including for current-position actions.
        let current = || Self::current_quartz_point();
        match action {
            ComputerAction::LeftClick { x, y } => self.click(
                self.map_point(ScreenPoint { x: *x, y: *y })?,
                CG_MOUSE_LEFT,
                CG_EVENT_LEFT_DOWN,
                CG_EVENT_LEFT_UP,
                1,
                cancel,
            )?,
            ComputerAction::RightClick => self.click(
                current()?,
                CG_MOUSE_RIGHT,
                CG_EVENT_RIGHT_DOWN,
                CG_EVENT_RIGHT_UP,
                1,
                cancel,
            )?,
            ComputerAction::MiddleClick => self.click(
                current()?,
                CG_MOUSE_CENTER,
                CG_EVENT_OTHER_DOWN,
                CG_EVENT_OTHER_UP,
                1,
                cancel,
            )?,
            ComputerAction::DoubleClick => {
                let point = current()?;
                for click_state in [1, 2] {
                    self.click(
                        point,
                        CG_MOUSE_LEFT,
                        CG_EVENT_LEFT_DOWN,
                        CG_EVENT_LEFT_UP,
                        click_state,
                        cancel,
                    )?;
                }
            }
            ComputerAction::LeftMouseDown => {
                let point = current()?;
                Self::post_mouse(CG_EVENT_LEFT_DOWN, point, CG_MOUSE_LEFT, 1)?;
                if let Err(error) = self.claim_left_down() {
                    if Self::post_mouse(CG_EVENT_LEFT_UP, point, CG_MOUSE_LEFT, 1).is_ok() {
                        let _ = self.clear_left_down();
                    }
                    return Err(error);
                }
            }
            ComputerAction::LeftMouseUp => {
                let point = current()?;
                self.post_left_up(point, 1)?;
            }
            ComputerAction::MouseMove { x, y } => {
                Self::post_mouse(
                    mouse_move_event_type(left_held_by_self),
                    self.map_point(ScreenPoint { x: *x, y: *y })?,
                    CG_MOUSE_LEFT,
                    i64::from(left_held_by_self),
                )?;
            }
            ComputerAction::LeftClickDrag { from, to } => {
                let from = self.map_point(*from)?;
                let to = self.map_point(*to)?;
                Self::post_mouse(CG_EVENT_MOUSE_MOVED, from, CG_MOUSE_LEFT, 0)?;
                cancel.check()?;
                Self::post_mouse(CG_EVENT_LEFT_DOWN, from, CG_MOUSE_LEFT, 1)?;
                if let Err(error) = self.claim_left_down() {
                    if Self::post_mouse(CG_EVENT_LEFT_UP, from, CG_MOUSE_LEFT, 1).is_ok() {
                        let _ = self.clear_left_down();
                    }
                    return Err(error);
                }
                let result = cancel
                    .check()
                    .and_then(|()| Self::post_mouse(CG_EVENT_LEFT_DRAGGED, to, CG_MOUSE_LEFT, 1))
                    .and_then(|()| self.post_left_up(to, 1));
                if result.is_err() {
                    let _ = self.post_left_up(to, 1);
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
            ComputerAction::Screenshot
            | ComputerAction::CursorPosition
            | ComputerAction::Inspect { .. } => {
                unreachable!("observe actions do not enter control")
            }
        }
        Ok(ComputerOutput::Confirmed {
            action: action_name(action).into(),
        })
    }
}

fn action_allowed_while_left_held(action: &ComputerAction) -> bool {
    matches!(
        action,
        ComputerAction::LeftMouseUp | ComputerAction::MouseMove { .. }
    )
}

fn mouse_move_event_type(left_held: bool) -> u32 {
    if left_held {
        CG_EVENT_LEFT_DRAGGED
    } else {
        CG_EVENT_MOUSE_MOVED
    }
}

fn map_delivered_pixel(viewport: Viewport, point: ScreenPoint) -> ComputerResult<CGPoint> {
    if point.x >= viewport.image_width || point.y >= viewport.image_height {
        return Err(ComputerError::InvalidAction {
            message: format!(
                "computer coordinate ({}, {}) is outside the delivered {}x{} screenshot",
                point.x, point.y, viewport.image_width, viewport.image_height
            ),
        });
    }
    Ok(CGPoint {
        x: viewport.display_bounds.origin.x
            + f64::from(point.x) * viewport.display_bounds.size.width
                / f64::from(viewport.image_width),
        y: viewport.display_bounds.origin.y
            + f64::from(point.y) * viewport.display_bounds.size.height
                / f64::from(viewport.image_height),
    })
}

fn copy_ax_attribute(
    element: AXUIElementRef,
    attribute: &CStr,
) -> ComputerResult<Option<CFTypeRef>> {
    // SAFETY: the C string is NUL-terminated for the full call and the
    // returned Core Foundation string is retained and released below.
    let attribute_name = unsafe {
        CFStringCreateWithCString(ptr::null(), attribute.as_ptr(), CF_STRING_ENCODING_UTF8)
    };
    if attribute_name.is_null() {
        return Err(ComputerError::Backend {
            message: "macOS could not construct an accessibility attribute name".into(),
        });
    }
    let mut value: CFTypeRef = ptr::null();
    // SAFETY: both element and attribute-name objects are live and `value` is
    // a writable out pointer. A successful Copy transfers one retain to us.
    let ax_error = unsafe { AXUIElementCopyAttributeValue(element, attribute_name, &mut value) };
    // SAFETY: balance the Create rule for the attribute-name string.
    unsafe { CFRelease(attribute_name) };
    match ax_error {
        AX_ERROR_SUCCESS if value.is_null() => Err(ComputerError::Backend {
            message: format!(
                "macOS accessibility attribute `{}` returned a null value",
                attribute.to_string_lossy()
            ),
        }),
        AX_ERROR_SUCCESS => Ok(Some(value)),
        AX_ERROR_ATTRIBUTE_UNSUPPORTED | AX_ERROR_NO_VALUE => Ok(None),
        error => Err(ComputerError::Backend {
            message: format!(
                "macOS could not read accessibility attribute `{}` (AXError {error})",
                attribute.to_string_lossy()
            ),
        }),
    }
}

fn copy_ax_text_attribute(
    element: AXUIElementRef,
    attribute: &CStr,
) -> ComputerResult<Option<String>> {
    let Some(value) = copy_ax_attribute(element, attribute)? else {
        return Ok(None);
    };
    let text = (|| {
        // SAFETY: `value` is a live CF object copied above; type-id APIs do not
        // alter ownership.
        let is_string = unsafe { CFGetTypeID(value) == CFStringGetTypeID() };
        if is_string {
            cf_string_to_string(value)
        } else {
            // Non-string AXValue payloads (for example numeric slider values)
            // still receive a stable textual representation.
            // SAFETY: `value` is live; Copy returns a retained CFString.
            let description = unsafe { CFCopyDescription(value) };
            if description.is_null() {
                return Err(ComputerError::Backend {
                    message: format!(
                        "macOS could not describe accessibility attribute `{}`",
                        attribute.to_string_lossy()
                    ),
                });
            }
            let text = cf_string_to_string(description);
            // SAFETY: balance the Copy rule for the description string.
            unsafe { CFRelease(description) };
            text
        }
    })();
    // SAFETY: balance the successful attribute Copy exactly once.
    unsafe { CFRelease(value) };
    text.map(Some)
}

fn cf_string_to_string(string: CFStringRef) -> ComputerResult<String> {
    // SAFETY: `string` is a live CFString for these read-only calls.
    let length = unsafe { CFStringGetLength(string) };
    let max_bytes = unsafe { CFStringGetMaximumSizeForEncoding(length, CF_STRING_ENCODING_UTF8) };
    let capacity = max_bytes
        .checked_add(1)
        .and_then(|length| usize::try_from(length).ok())
        .ok_or_else(|| ComputerError::Backend {
            message: "macOS accessibility string is too large to encode".into(),
        })?;
    let mut buffer = vec![0_u8; capacity];
    let buffer_size = isize::try_from(capacity).map_err(|_| ComputerError::Backend {
        message: "macOS accessibility string buffer exceeds the CFIndex range".into(),
    })?;
    // SAFETY: `buffer` is writable for `buffer_size` bytes and CFString writes
    // a terminating NUL when conversion succeeds.
    let converted = unsafe {
        CFStringGetCString(
            string,
            buffer.as_mut_ptr().cast(),
            buffer_size,
            CF_STRING_ENCODING_UTF8,
        ) != 0
    };
    if !converted {
        return Err(ComputerError::Backend {
            message: "macOS could not encode an accessibility string as UTF-8".into(),
        });
    }
    // SAFETY: successful CFStringGetCString guarantees the terminating NUL.
    Ok(unsafe { CStr::from_ptr(buffer.as_ptr().cast()) }
        .to_string_lossy()
        .into_owned())
}

fn copy_ax_bounds(element: AXUIElementRef) -> ComputerResult<Option<CGRect>> {
    let Some(value) = copy_ax_attribute(element, c"AXFrame")? else {
        return Ok(None);
    };
    let bounds = (|| {
        // SAFETY: `value` is live and these type-id calls do not alter it.
        if unsafe { CFGetTypeID(value) != AXValueGetTypeID() } {
            return Err(ComputerError::Backend {
                message: "macOS AXFrame attribute was not an AXValue".into(),
            });
        }
        let mut bounds = CGRect {
            origin: CGPoint { x: 0.0, y: 0.0 },
            size: CGSize {
                width: 0.0,
                height: 0.0,
            },
        };
        // SAFETY: `bounds` has the exact CoreGraphics CGRect ABI requested by
        // AXValue and remains writable for the synchronous call.
        if unsafe { AXValueGetValue(value, AX_VALUE_CGRECT, (&mut bounds as *mut CGRect).cast()) }
            == 0
        {
            return Err(ComputerError::Backend {
                message: "macOS AXFrame attribute did not contain a CGRect".into(),
            });
        }
        Ok(bounds)
    })();
    // SAFETY: balance the successful AXFrame attribute Copy exactly once.
    unsafe { CFRelease(value) };
    bounds.map(Some)
}

fn map_accessibility_bounds(
    viewport: Viewport,
    bounds: CGRect,
) -> Option<ComputerInspectionBounds> {
    let display = viewport.display_bounds;
    if ![
        bounds.origin.x,
        bounds.origin.y,
        bounds.size.width,
        bounds.size.height,
        display.size.width,
        display.size.height,
    ]
    .into_iter()
    .all(f64::is_finite)
        || bounds.size.width < 0.0
        || bounds.size.height < 0.0
        || display.size.width <= 0.0
        || display.size.height <= 0.0
    {
        return None;
    }
    let scale_x = f64::from(viewport.image_width) / display.size.width;
    let scale_y = f64::from(viewport.image_height) / display.size.height;
    let left = ((bounds.origin.x - display.origin.x) * scale_x)
        .floor()
        .clamp(0.0, f64::from(viewport.image_width));
    let top = ((bounds.origin.y - display.origin.y) * scale_y)
        .floor()
        .clamp(0.0, f64::from(viewport.image_height));
    let right = ((bounds.origin.x + bounds.size.width - display.origin.x) * scale_x)
        .ceil()
        .clamp(left, f64::from(viewport.image_width));
    let bottom = ((bounds.origin.y + bounds.size.height - display.origin.y) * scale_y)
        .ceil()
        .clamp(top, f64::from(viewport.image_height));
    let x = left as u32;
    let y = top as u32;
    Some(ComputerInspectionBounds {
        x,
        y,
        width: (right as u32).saturating_sub(x),
        height: (bottom as u32).saturating_sub(y),
    })
}

#[async_trait]
impl ComputerBackend for MacOsComputerBackend {
    async fn prepare(
        &self,
        action: &ComputerAction,
        cancel: &ComputerCancelToken,
    ) -> ComputerResult<()> {
        cancel.check()?;
        match action {
            ComputerAction::Screenshot | ComputerAction::CursorPosition => {
                self.preflight_screen_recording()
            }
            ComputerAction::Inspect { .. } => {
                self.preflight_screen_recording()?;
                self.preflight_accessibility()
            }
            ComputerAction::Wait { .. }
            | ComputerAction::LeftClick { .. }
            | ComputerAction::RightClick
            | ComputerAction::MiddleClick
            | ComputerAction::DoubleClick
            | ComputerAction::LeftMouseDown
            | ComputerAction::LeftMouseUp
            | ComputerAction::MouseMove { .. }
            | ComputerAction::LeftClickDrag { .. }
            | ComputerAction::Type { .. }
            | ComputerAction::Key { .. }
            | ComputerAction::Scroll { .. } => self.preflight_accessibility(),
        }
    }

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
            ComputerAction::Inspect { x, y } => {
                let inspection = self.inspect(*x, *y, cancel)?;
                let screenshot_png = self.capture_png(cancel).await?;
                Ok(ComputerOutput::Inspection {
                    inspection,
                    screenshot_png,
                })
            }
            ComputerAction::Wait { ms } => {
                self.preflight_accessibility()?;
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

    async fn poll_permission(
        &self,
        permission: SystemPermission,
        cancel: &ComputerCancelToken,
        timeout: Duration,
    ) -> ComputerResult<ComputerPermissionPoll> {
        const POLL_INTERVAL: Duration = Duration::from_millis(250);

        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            cancel.check()?;
            let granted = match permission {
                SystemPermission::ScreenRecording => {
                    // SAFETY: parameterless CoreGraphics TCC preflight; no
                    // pointer ownership crosses this call.
                    unsafe { CGPreflightScreenCaptureAccess() }
                }
                SystemPermission::Accessibility => Self::accessibility_trusted(false)?,
            };
            if granted {
                return Ok(match permission {
                    // A Screen Recording toggle granted after this process's
                    // failed preflight is not consumed speculatively. Leave
                    // the action undispatched and resume it in a fresh daemon.
                    SystemPermission::ScreenRecording => ComputerPermissionPoll::RestartRequired,
                    SystemPermission::Accessibility => ComputerPermissionPoll::Granted,
                });
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Ok(ComputerPermissionPoll::TimedOut);
            }
            let delay = POLL_INTERVAL.min(deadline.saturating_duration_since(now));
            tokio::select! {
                () = tokio::time::sleep(delay) => {}
                () = wait_for_cancel(cancel) => return Err(ComputerError::Cancelled),
            }
        }
    }

    fn set_viewport(&self, width: u32, height: u32) -> ComputerResult<()> {
        if width == 0 || height == 0 {
            return Err(ComputerError::InvalidAction {
                message: "CU-1 returned an empty computer screenshot viewport".into(),
            });
        }
        let mut state = self.lock_state()?;
        let bounds = state
            .pending_display_bounds
            .ok_or_else(|| ComputerError::Backend {
                message: "CU-1 viewport arrived without a matching macOS capture".into(),
            })?;
        state.viewport = Some(Viewport {
            display_bounds: bounds,
            image_width: width,
            image_height: height,
        });
        Ok(())
    }

    async fn emergency_stop(&self) -> ComputerResult<()> {
        let _input_gate = INPUT_GATE.lock().await;
        // The global owner is authoritative even if a local-state update
        // failed after the physical down event crossed into WindowServer.
        if Self::held_left_owner()? == Some(self.input_owner) {
            let point = Self::current_quartz_point()?;
            self.post_left_up(point, 1)?;
        }
        Ok(())
    }
}

async fn wait_for_cancel(cancel: &ComputerCancelToken) {
    while !cancel.is_cancelled() {
        tokio::time::sleep(Duration::from_millis(5)).await;
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

fn virtual_key_code(key: &str) -> Option<u16> {
    Some(match key.to_ascii_lowercase().as_str() {
        "a" => 0,
        "s" => 1,
        "d" => 2,
        "f" => 3,
        "h" => 4,
        "g" => 5,
        "z" => 6,
        "x" => 7,
        "c" => 8,
        "v" => 9,
        "b" => 11,
        "q" => 12,
        "w" => 13,
        "e" => 14,
        "r" => 15,
        "y" => 16,
        "t" => 17,
        "1" => 18,
        "2" => 19,
        "3" => 20,
        "4" => 21,
        "6" => 22,
        "5" => 23,
        "=" | "plus" => 24,
        "9" => 25,
        "7" => 26,
        "-" | "minus" => 27,
        "8" => 28,
        "0" => 29,
        "]" => 30,
        "o" => 31,
        "u" => 32,
        "[" => 33,
        "i" => 34,
        "p" => 35,
        "return" | "enter" => 36,
        "l" => 37,
        "j" => 38,
        "'" => 39,
        "k" => 40,
        ";" => 41,
        "\\" => 42,
        "," => 43,
        "/" => 44,
        "n" => 45,
        "m" => 46,
        "." => 47,
        "tab" => 48,
        "space" => 49,
        "`" => 50,
        "backspace" | "delete" => 51,
        "escape" | "esc" => 53,
        "left" => 123,
        "right" => 124,
        "down" => 125,
        "up" => 126,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivered_cu1_pixels_map_to_quartz_points_without_hardcoded_retina_scale() {
        let viewport = Viewport {
            display_bounds: CGRect {
                origin: CGPoint { x: 100.0, y: 50.0 },
                size: CGSize {
                    width: 1_440.0,
                    height: 900.0,
                },
            },
            // A 3,000x1,875 Retina capture admitted by CU-1 at 2,048x1,280.
            image_width: 2_048,
            image_height: 1_280,
        };
        let point = match map_delivered_pixel(viewport, ScreenPoint { x: 1_024, y: 640 }) {
            Ok(point) => point,
            Err(error) => panic!("center must map: {error}"),
        };
        assert_eq!(point.x, 820.0);
        assert_eq!(point.y, 500.0);
        assert!(map_delivered_pixel(viewport, ScreenPoint { x: 2_048, y: 0 }).is_err());
    }

    #[test]
    fn accessibility_bounds_map_back_to_the_delivered_cu1_viewport() {
        let viewport = Viewport {
            display_bounds: CGRect {
                origin: CGPoint { x: 100.0, y: 50.0 },
                size: CGSize {
                    width: 1_440.0,
                    height: 900.0,
                },
            },
            image_width: 2_048,
            image_height: 1_280,
        };
        assert_eq!(
            map_accessibility_bounds(
                viewport,
                CGRect {
                    origin: CGPoint { x: 820.0, y: 500.0 },
                    size: CGSize {
                        width: 144.0,
                        height: 90.0,
                    },
                },
            ),
            Some(ComputerInspectionBounds {
                x: 1_024,
                y: 640,
                width: 205,
                height: 128,
            })
        );

        assert_eq!(
            map_accessibility_bounds(
                viewport,
                CGRect {
                    origin: CGPoint {
                        x: 1_480.0,
                        y: 100.0,
                    },
                    size: CGSize {
                        width: 100.0,
                        height: 20.0,
                    },
                },
            ),
            Some(ComputerInspectionBounds {
                x: 1_962,
                y: 71,
                width: 86,
                height: 29,
            }),
            "horizontal AX bounds clamp against admitted width, not height"
        );
    }

    #[test]
    fn held_left_button_only_allows_drag_motion_or_release() {
        assert!(action_allowed_while_left_held(&ComputerAction::MouseMove {
            x: 1,
            y: 2
        }));
        assert!(action_allowed_while_left_held(&ComputerAction::LeftMouseUp));
        assert!(!action_allowed_while_left_held(
            &ComputerAction::LeftClick { x: 1, y: 2 }
        ));
        assert_eq!(mouse_move_event_type(false), CG_EVENT_MOUSE_MOVED);
        assert_eq!(mouse_move_event_type(true), CG_EVENT_LEFT_DRAGGED);
    }

    /// Real display/TCC exercise for owner dogfood only. CI and ordinary test
    /// runs use the fake backend seam and never call protected hardware APIs.
    #[tokio::test]
    #[ignore = "requires an interactive macOS display and Screen Recording permission"]
    async fn manual_real_hardware_screenshot() {
        let backend = MacOsComputerBackend::new();
        let output = match backend
            .execute(&ComputerAction::Screenshot, &ComputerCancelToken::new())
            .await
        {
            Ok(output) => output,
            Err(error) => panic!("manual macOS screenshot failed: {error}"),
        };
        assert!(matches!(output, ComputerOutput::ScreenshotPng(bytes) if !bytes.is_empty()));
    }
}
