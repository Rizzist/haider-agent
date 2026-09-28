//! Windows desktop presence overlay (`haiderd.exe --cu-presence-overlay`).
//!
//! Three layered, topmost, tool (no taskbar button), no-activate popups:
//!
//! * the agent pointer and the click ring are `WS_EX_TRANSPARENT`
//!   (click-through) and drawn with `UpdateLayeredWindow` from the shared
//!   premultiplied art;
//! * the "Haider is controlling this screen · Stop" badge is clickable but
//!   answers `WM_MOUSEACTIVATE` with `MA_NOACTIVATE`, so pressing Stop never
//!   takes keyboard focus from the app under control.
//!
//! Every window calls `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)`
//! (Windows 10 2004+), which removes it from screen capture, including the
//! backend's `BitBlt` of the desktop DC. When the OS refuses,
//! `Ready.capture_excluded` is `false` and the daemon logs it.
//!
//! A Stop click whose `GetMessageExtraInfo()` equals
//! [`SYNTHETIC_INPUT_TAG`] was injected by Haider's own `SendInput` and is
//! ignored. Every FFI call is confined to the `win` module; each unsafe block
//! states its invariant.

use super::{
    PresenceCommand, PresenceEvent, PresenceMark, PresencePoint, SYNTHETIC_INPUT_TAG, art,
    encode_event, parse_command_line,
};
use std::collections::VecDeque;
use std::io::{BufRead as _, Write as _};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MOVE_DURATION: Duration = Duration::from_millis(160);
const RING_DURATION: Duration = Duration::from_millis(450);
const BADGE_MARGIN: f64 = 10.0;
const BADGE_AVOID_MARGIN: f64 = 28.0;

/// Set by the badge's window procedure; drained by the main loop.
static STOP_CLICKED: AtomicBool = AtomicBool::new(false);
static SYNTHETIC_STOP_IGNORED: AtomicBool = AtomicBool::new(false);

pub(super) fn run() -> i32 {
    win::per_monitor_dpi_awareness();
    let queue = Arc::new(Mutex::new(VecDeque::new()));
    let closed = Arc::new(AtomicBool::new(false));
    let reader_queue = Arc::clone(&queue);
    let reader_closed = Arc::clone(&closed);
    let spawned = std::thread::Builder::new()
        .name("presence-stdin".into())
        .spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                match parse_command_line(&line) {
                    Some(Ok(command)) => {
                        if let Ok(mut queue) = reader_queue.lock() {
                            queue.push_back(command);
                        }
                    }
                    Some(Err(message)) => emit(&PresenceEvent::Error {
                        message: format!("ignored malformed presence command: {message}"),
                    }),
                    None => {}
                }
            }
            reader_closed.store(true, Ordering::Release);
        });
    if spawned.is_err() {
        return 70;
    }
    let mut overlay = match Overlay::new() {
        Ok(overlay) => overlay,
        Err(message) => {
            emit(&PresenceEvent::Error { message });
            return 70;
        }
    };
    emit(&PresenceEvent::Ready {
        platform: "windows".into(),
        capture_excluded: overlay.capture_excluded,
    });
    loop {
        win::pump_messages_for(Duration::from_millis(16));
        if STOP_CLICKED.swap(false, Ordering::AcqRel) && overlay.visible && !overlay.stopping {
            overlay.stopping = true;
            emit(&PresenceEvent::Stop);
        }
        if SYNTHETIC_STOP_IGNORED.swap(false, Ordering::AcqRel) {
            emit(&PresenceEvent::Error {
                message: "ignored a Stop click synthesised by Haider".into(),
            });
        }
        let pending: Vec<PresenceCommand> = queue
            .lock()
            .map(|mut queue| queue.drain(..).collect())
            .unwrap_or_default();
        for command in pending {
            overlay.apply(command);
        }
        overlay.animate(Instant::now());
        if closed.load(Ordering::Acquire) {
            overlay.hide();
            return 0;
        }
    }
}

fn emit(event: &PresenceEvent) {
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{}", encode_event(event));
    let _ = stdout.flush();
}

type BadgeText<'a> = (&'a str, (f64, f64, f64, f64), bool);

struct Overlay {
    scale: f64,
    pointer: win::Layered,
    ring: win::Layered,
    badge: win::Layered,
    ring_bitmap: art::Bitmap,
    capture_excluded: bool,
    visible: bool,
    stopping: bool,
    badge_at_top: bool,
    label: String,
    position: Option<(f64, f64)>,
    move_from: (f64, f64),
    move_to: (f64, f64),
    move_started: Option<Instant>,
    ack_on_arrival: Option<u64>,
    ring_on_arrival: bool,
    ring_started: Option<Instant>,
}

impl Overlay {
    fn new() -> Result<Self, String> {
        let scale = win::system_scale();
        let pointer = win::Layered::create(true)?;
        let ring = win::Layered::create(true)?;
        let badge = win::Layered::create(false)?;
        let capture_excluded = [&pointer, &ring, &badge]
            .iter()
            .all(|window| window.exclude_from_capture());
        pointer.set_image(&art::pointer(scale), 255)?;
        let ring_bitmap = art::click_ring(scale);
        ring.set_image(&ring_bitmap, 255)?;
        let mut overlay = Self {
            scale,
            pointer,
            ring,
            badge,
            ring_bitmap,
            capture_excluded,
            visible: false,
            stopping: false,
            badge_at_top: true,
            label: "Haider is controlling this screen".into(),
            position: None,
            move_from: (0.0, 0.0),
            move_to: (0.0, 0.0),
            move_started: None,
            ack_on_arrival: None,
            ring_on_arrival: false,
            ring_started: None,
        };
        overlay.draw_badge("Stop")?;
        Ok(overlay)
    }

    fn draw_badge(&mut self, stop: &str) -> Result<(), String> {
        let bitmap = art::badge(self.scale);
        let (stop_x, stop_y, stop_w, stop_h) = art::BADGE_STOP_RECT;
        let s = self.scale;
        let label = self.label.clone();
        let texts: [BadgeText<'_>; 2] = [
            (
                label.as_str(),
                (32.0 * s, 0.0, 294.0 * s, art::BADGE_SIZE.1 * s),
                false,
            ),
            (
                stop,
                (
                    stop_x * s,
                    stop_y * s,
                    (stop_x + stop_w) * s,
                    (stop_y + stop_h) * s,
                ),
                true,
            ),
        ];
        self.badge
            .set_image_with_text(&bitmap, &texts, (13.0 * s).round() as i32)
    }

    fn badge_origin(&self, top: bool) -> (i32, i32) {
        let work = win::work_area();
        let width = art::BADGE_SIZE.0 * self.scale;
        let height = art::BADGE_SIZE.1 * self.scale;
        let x = f64::from(work.0) + (f64::from(work.2 - work.0) - width) / 2.0;
        let y = if top {
            f64::from(work.1) + BADGE_MARGIN * self.scale
        } else {
            f64::from(work.3) - height - BADGE_MARGIN * self.scale
        };
        (x.round() as i32, y.round() as i32)
    }

    fn apply(&mut self, command: PresenceCommand) {
        match command {
            PresenceCommand::Show { label, .. } => {
                self.label = label;
                self.stopping = false;
                let _ = self.draw_badge("Stop");
                self.visible = true;
                let (x, y) = self.badge_origin(self.badge_at_top);
                self.badge.show_at(x, y);
                if let Some(position) = self.position {
                    self.place_pointer(position);
                }
            }
            PresenceCommand::Pointer {
                seq, mark, point, ..
            } => self.pointer_to(seq, mark, point),
            PresenceCommand::Stopping { .. } => {
                self.stopping = true;
                let _ = self.draw_badge("…");
            }
            PresenceCommand::Hide { .. } => self.hide(),
            // The daemon also conceals before it has read `Ready`; windows
            // with WDA_EXCLUDEFROMCAPTURE are already absent from captures.
            PresenceCommand::Conceal { seq, .. } => {
                if !self.capture_excluded {
                    self.pointer.hide();
                    self.ring.hide();
                    self.badge.hide();
                }
                emit(&PresenceEvent::Ack { seq });
            }
            PresenceCommand::Reveal { .. } => {
                if self.visible && !self.capture_excluded {
                    let (x, y) = self.badge_origin(self.badge_at_top);
                    self.badge.show_at(x, y);
                    if let Some(position) = self.position {
                        self.place_pointer(position);
                    }
                }
            }
        }
    }

    fn pointer_to(&mut self, seq: u64, mark: PresenceMark, point: Option<PresencePoint>) {
        let Some(target) = point.map(|point| (point.x, point.y)).or(self.position) else {
            emit(&PresenceEvent::Ack { seq });
            return;
        };
        if mark.posts_pointer_input() {
            self.avoid_badge(target);
        }
        let from = self.position.unwrap_or(target);
        self.move_from = from;
        self.move_to = target;
        self.move_started = Some(Instant::now());
        self.ring_on_arrival = mark.posts_pointer_input();
        if mark.posts_pointer_input() && from != target {
            if let Some(previous) = self.ack_on_arrival.replace(seq) {
                emit(&PresenceEvent::Ack { seq: previous });
            }
        } else {
            emit(&PresenceEvent::Ack { seq });
        }
        self.position = Some(target);
        self.animate(Instant::now());
    }

    fn avoid_badge(&mut self, target: (f64, f64)) {
        let (x, y) = self.badge_origin(self.badge_at_top);
        let margin = BADGE_AVOID_MARGIN * self.scale;
        let near = target.0 >= f64::from(x) - margin
            && target.0 <= f64::from(x) + art::BADGE_SIZE.0 * self.scale + margin
            && target.1 >= f64::from(y) - margin
            && target.1 <= f64::from(y) + art::BADGE_SIZE.1 * self.scale + margin;
        if near {
            self.badge_at_top = !self.badge_at_top;
            if self.visible {
                let (x, y) = self.badge_origin(self.badge_at_top);
                self.badge.show_at(x, y);
            }
        }
    }

    fn place_pointer(&self, point: (f64, f64)) {
        if !self.visible {
            return;
        }
        let tip = art::POINTER_TIP * self.scale;
        self.pointer.show_at(
            (point.0 - tip).round() as i32,
            (point.1 - tip).round() as i32,
        );
    }

    fn animate(&mut self, now: Instant) {
        if let Some(started) = self.move_started {
            let progress = (now.saturating_duration_since(started).as_secs_f64()
                / MOVE_DURATION.as_secs_f64())
            .min(1.0);
            let eased = 1.0 - (1.0 - progress).powi(3);
            self.place_pointer((
                self.move_from.0 + (self.move_to.0 - self.move_from.0) * eased,
                self.move_from.1 + (self.move_to.1 - self.move_from.1) * eased,
            ));
            if progress >= 1.0 {
                self.move_started = None;
                if let Some(seq) = self.ack_on_arrival.take() {
                    emit(&PresenceEvent::Ack { seq });
                }
                if std::mem::take(&mut self.ring_on_arrival) && self.visible {
                    let half = art::RING_SIZE * self.scale / 2.0;
                    self.ring.show_at(
                        (self.move_to.0 - half).round() as i32,
                        (self.move_to.1 - half).round() as i32,
                    );
                    self.ring_started = Some(now);
                }
            }
        }
        if let Some(started) = self.ring_started {
            let progress =
                now.saturating_duration_since(started).as_secs_f64() / RING_DURATION.as_secs_f64();
            if progress >= 1.0 {
                self.ring.hide();
                self.ring_started = None;
            } else {
                let alpha = ((1.0 - progress) * 255.0).round() as u8;
                let _ = self.ring.set_image(&self.ring_bitmap, alpha);
            }
        }
    }

    fn hide(&mut self) {
        self.visible = false;
        self.stopping = false;
        self.move_started = None;
        self.ring_started = None;
        if let Some(seq) = self.ack_on_arrival.take() {
            emit(&PresenceEvent::Ack { seq });
        }
        self.pointer.hide();
        self.ring.hide();
        self.badge.hide();
    }
}

/// All Win32 FFI for the overlay.
mod win {
    use super::{BadgeText, STOP_CLICKED, SYNTHETIC_INPUT_TAG, SYNTHETIC_STOP_IGNORED, art};
    use std::ptr;
    use std::sync::atomic::{AtomicPtr, AtomicU32, Ordering};
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::{
        AC_SRC_ALPHA, AC_SRC_OVER, ANTIALIASED_QUALITY, BI_RGB, BITMAPINFO, BITMAPINFOHEADER,
        BLENDFUNCTION, CLIP_DEFAULT_PRECIS, CreateCompatibleDC, CreateDIBSection, CreateFontW,
        DEFAULT_CHARSET, DEFAULT_PITCH, DIB_RGB_COLORS, DT_CENTER, DT_LEFT, DT_SINGLELINE,
        DT_VCENTER, DeleteDC, DeleteObject, DrawTextW, FW_SEMIBOLD, GetDC, OUT_DEFAULT_PRECIS,
        ReleaseDC, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
    };
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForSystem, SetProcessDpiAwarenessContext,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageExtraInfo, HWND_TOPMOST,
        MA_NOACTIVATE, MSG, MsgWaitForMultipleObjects, PM_REMOVE, PeekMessageW, QS_ALLINPUT,
        RegisterClassExW, SPI_GETWORKAREA, SW_HIDE, SWP_NOACTIVATE, SWP_NOSIZE, SWP_SHOWWINDOW,
        SetWindowDisplayAffinity, SetWindowPos, ShowWindow, SystemParametersInfoW,
        TranslateMessage, ULW_ALPHA, UpdateLayeredWindow, WDA_EXCLUDEFROMCAPTURE, WM_LBUTTONDOWN,
        WM_MOUSEACTIVATE, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
        WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
    };

    static BADGE: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(ptr::null_mut());
    static BADGE_SCALE_MILLI: AtomicU32 = AtomicU32::new(1000);

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub(super) fn per_monitor_dpi_awareness() {
        // SAFETY: process-wide flag set before any window exists; failure
        // (already set) is harmless.
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    }

    pub(super) fn system_scale() -> f64 {
        // SAFETY: no arguments; returns the system DPI.
        let dpi = unsafe { GetDpiForSystem() };
        if dpi == 0 { 1.0 } else { f64::from(dpi) / 96.0 }
    }

    /// Primary work area (left, top, right, bottom) in physical pixels.
    pub(super) fn work_area() -> (i32, i32, i32, i32) {
        let mut rect = RECT::default();
        // SAFETY: `rect` is valid writable storage for SPI_GETWORKAREA.
        let ok = unsafe { SystemParametersInfoW(SPI_GETWORKAREA, 0, (&raw mut rect).cast(), 0) };
        if ok == 0 {
            (0, 0, 1920, 1080)
        } else {
            (rect.left, rect.top, rect.right, rect.bottom)
        }
    }

    /// Dispatches this thread's window messages for up to `budget`.
    pub(super) fn pump_messages_for(budget: Duration) {
        let deadline = Instant::now() + budget;
        loop {
            let mut message = MSG::default();
            // SAFETY: `message` is valid storage; a null HWND drains every
            // window of this thread; each MSG is dispatched on this thread.
            unsafe {
                while PeekMessageW(&raw mut message, ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                    TranslateMessage(&raw const message);
                    DispatchMessageW(&raw const message);
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return;
            }
            let millis = u32::try_from(remaining.as_millis()).unwrap_or(16);
            // SAFETY: zero handles; waits only for input or the timeout.
            unsafe { MsgWaitForMultipleObjects(0, ptr::null(), 0, millis, QS_ALLINPUT) };
        }
    }

    extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == WM_MOUSEACTIVATE {
            return MA_NOACTIVATE as LRESULT;
        }
        if message == WM_LBUTTONDOWN && hwnd == BADGE.load(Ordering::Acquire) {
            // SAFETY: reads the extra info of the message being processed.
            let extra = unsafe { GetMessageExtraInfo() };
            if i64::try_from(extra).is_ok_and(|extra| extra == SYNTHETIC_INPUT_TAG) {
                SYNTHETIC_STOP_IGNORED.store(true, Ordering::Release);
                return 0;
            }
            let scale = f64::from(BADGE_SCALE_MILLI.load(Ordering::Acquire)) / 1000.0;
            let x = f64::from((lparam & 0xFFFF) as u16 as i16) / scale;
            let y = f64::from(((lparam >> 16) & 0xFFFF) as u16 as i16) / scale;
            if art::badge_stop_hit(x, y) {
                STOP_CLICKED.store(true, Ordering::Release);
            }
            return 0;
        }
        // SAFETY: default processing for this window's own message.
        unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
    }

    fn class_name() -> Vec<u16> {
        let name = wide("HaiderPresenceOverlay");
        let class = WNDCLASSEXW {
            cbSize: u32::try_from(std::mem::size_of::<WNDCLASSEXW>()).unwrap_or(0),
            lpfnWndProc: Some(window_proc),
            // SAFETY: a null module name returns this executable's handle.
            hInstance: unsafe { GetModuleHandleW(ptr::null()) },
            lpszClassName: name.as_ptr(),
            ..WNDCLASSEXW::default()
        };
        // SAFETY: `class` and the name it points to outlive the call; a
        // repeated registration fails harmlessly (the class already exists).
        unsafe { RegisterClassExW(&raw const class) };
        name
    }

    /// One layered popup.
    pub(super) struct Layered {
        hwnd: HWND,
    }

    impl Layered {
        pub(super) fn create(click_through: bool) -> Result<Self, String> {
            let class = class_name();
            let mut ex_style = WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
            if click_through {
                ex_style |= WS_EX_TRANSPARENT;
            }
            let title = wide("Haider presence");
            // SAFETY: class/title are NUL-terminated and outlive the call;
            // null parent/menu/param are valid for a top-level popup.
            let hwnd = unsafe {
                CreateWindowExW(
                    ex_style,
                    class.as_ptr(),
                    title.as_ptr(),
                    WS_POPUP,
                    0,
                    0,
                    1,
                    1,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    GetModuleHandleW(ptr::null()),
                    ptr::null(),
                )
            };
            if hwnd.is_null() {
                return Err("Windows refused to create the presence overlay window".into());
            }
            if !click_through {
                BADGE.store(hwnd, Ordering::Release);
            }
            Ok(Self { hwnd })
        }

        pub(super) fn exclude_from_capture(&self) -> bool {
            // SAFETY: `hwnd` is this thread's live top-level window.
            unsafe { SetWindowDisplayAffinity(self.hwnd, WDA_EXCLUDEFROMCAPTURE) != 0 }
        }

        pub(super) fn set_image(&self, bitmap: &art::Bitmap, alpha: u8) -> Result<(), String> {
            self.update(bitmap, &[], 0, alpha)
        }

        pub(super) fn set_image_with_text(
            &self,
            bitmap: &art::Bitmap,
            texts: &[BadgeText<'_>],
            font_px: i32,
        ) -> Result<(), String> {
            BADGE_SCALE_MILLI.store(
                (f64::from(bitmap.width) / art::BADGE_SIZE.0 * 1000.0).round() as u32,
                Ordering::Release,
            );
            self.update(bitmap, texts, font_px, 255)
        }

        fn update(
            &self,
            bitmap: &art::Bitmap,
            texts: &[BadgeText<'_>],
            font_px: i32,
            alpha: u8,
        ) -> Result<(), String> {
            let width = i32::try_from(bitmap.width).map_err(|_| "overlay too wide".to_owned())?;
            let height = i32::try_from(bitmap.height).map_err(|_| "overlay too tall".to_owned())?;
            let pixels = bitmap.premultiplied_bgra();
            // The DIB section below holds exactly `width * height * 4` bytes;
            // `art::Bitmap`'s fields are public, so check its invariant here
            // rather than trusting every constructor.
            let expected = usize::try_from(bitmap.width)
                .ok()
                .zip(usize::try_from(bitmap.height).ok())
                .and_then(|(w, h)| w.checked_mul(h))
                .and_then(|n| n.checked_mul(4));
            if expected != Some(pixels.len()) {
                return Err("overlay bitmap size does not match its dimensions".into());
            }
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: u32::try_from(std::mem::size_of::<BITMAPINFOHEADER>()).unwrap_or(40),
                    biWidth: width,
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB,
                    ..BITMAPINFOHEADER::default()
                },
                ..BITMAPINFO::default()
            };
            let face = wide("Segoe UI");
            let wide_texts: Vec<(Vec<u16>, RECT, bool)> = texts
                .iter()
                .map(|(text, (left, top, right, bottom), centered)| {
                    (
                        text.encode_utf16().collect(),
                        RECT {
                            left: left.round() as i32,
                            top: top.round() as i32,
                            right: right.round() as i32,
                            bottom: bottom.round() as i32,
                        },
                        *centered,
                    )
                })
                .collect();
            // SAFETY: the memory DC, DIB section and `bits` pointer are
            // null-checked before use; `GetDC(NULL)` and `CreateFontW` are not
            // (GDI treats a null DC/font as a failed no-op: DrawTextW and
            // SelectObject return 0, they do not dereference it). Every handle
            // is used on this thread only and released before returning.
            // `bits` points to `width * height * 4` bytes owned by the live DIB
            // section, which equals `pixels.len()` (checked above); it is
            // written only while the section is alive.
            let updated = unsafe {
                let screen = GetDC(ptr::null_mut());
                let memory = CreateCompatibleDC(screen);
                let mut bits: *mut core::ffi::c_void = ptr::null_mut();
                let dib = CreateDIBSection(
                    screen,
                    &raw const info,
                    DIB_RGB_COLORS,
                    &raw mut bits,
                    ptr::null_mut(),
                    0,
                );
                let updated = if dib.is_null() || bits.is_null() || memory.is_null() {
                    false
                } else {
                    ptr::copy_nonoverlapping(pixels.as_ptr(), bits.cast::<u8>(), pixels.len());
                    let previous = SelectObject(memory, dib);
                    if !wide_texts.is_empty() {
                        let font = CreateFontW(
                            -font_px,
                            0,
                            0,
                            0,
                            FW_SEMIBOLD as i32,
                            0,
                            0,
                            0,
                            u32::from(DEFAULT_CHARSET),
                            u32::from(OUT_DEFAULT_PRECIS),
                            u32::from(CLIP_DEFAULT_PRECIS),
                            u32::from(ANTIALIASED_QUALITY),
                            u32::from(DEFAULT_PITCH),
                            face.as_ptr(),
                        );
                        let previous_font = SelectObject(memory, font);
                        SetBkMode(memory, TRANSPARENT as i32);
                        SetTextColor(memory, 0x00FF_FFFF);
                        for (text, rect, centered) in &wide_texts {
                            let mut rect = *rect;
                            let align = if *centered { DT_CENTER } else { DT_LEFT };
                            DrawTextW(
                                memory,
                                text.as_ptr(),
                                i32::try_from(text.len()).unwrap_or(0),
                                &raw mut rect,
                                align | DT_VCENTER | DT_SINGLELINE,
                            );
                        }
                        SelectObject(memory, previous_font);
                        DeleteObject(font);
                        // GDI text clears alpha; restore the art's coverage so
                        // glyph pixels stay opaque over the pill.
                        let written =
                            std::slice::from_raw_parts_mut(bits.cast::<u8>(), pixels.len());
                        for (index, chunk) in written.chunks_exact_mut(4).enumerate() {
                            chunk[3] = pixels[index * 4 + 3];
                        }
                    }
                    let origin = POINT { x: 0, y: 0 };
                    let size = SIZE {
                        cx: width,
                        cy: height,
                    };
                    let blend = BLENDFUNCTION {
                        BlendOp: AC_SRC_OVER as u8,
                        BlendFlags: 0,
                        SourceConstantAlpha: alpha,
                        AlphaFormat: AC_SRC_ALPHA as u8,
                    };
                    let updated = UpdateLayeredWindow(
                        self.hwnd,
                        screen,
                        ptr::null(),
                        &raw const size,
                        memory,
                        &raw const origin,
                        0,
                        &raw const blend,
                        ULW_ALPHA,
                    ) != 0;
                    SelectObject(memory, previous);
                    updated
                };
                if !dib.is_null() {
                    DeleteObject(dib);
                }
                if !memory.is_null() {
                    DeleteDC(memory);
                }
                ReleaseDC(ptr::null_mut(), screen);
                updated
            };
            if updated {
                Ok(())
            } else {
                Err("UpdateLayeredWindow failed for the presence overlay".into())
            }
        }

        pub(super) fn show_at(&self, x: i32, y: i32) {
            // SAFETY: `hwnd` is live; the flags neither resize nor activate.
            unsafe {
                SetWindowPos(
                    self.hwnd,
                    HWND_TOPMOST,
                    x,
                    y,
                    0,
                    0,
                    SWP_NOACTIVATE | SWP_NOSIZE | SWP_SHOWWINDOW,
                );
            }
        }

        pub(super) fn hide(&self) {
            // SAFETY: `hwnd` is live.
            unsafe { ShowWindow(self.hwnd, SW_HIDE) };
        }
    }
}
