//! macOS desktop presence overlay (`haiderd --cu-presence-overlay`).
//!
//! Three borderless, non-activating `NSPanel`s at screen-saver level:
//!
//! * the agent pointer (gold arrow + caption), click-through;
//! * a click ring flashed at click targets, click-through;
//! * the "Haider is controlling this screen · Stop" badge, which accepts
//!   mouse clicks without activating the helper, so pressing Stop never
//!   steals keyboard focus from the app the agent (or the human) is using.
//!
//! Every panel sets `NSWindowSharingNone`, which removes it from window-server
//! screen capture (`CGDisplayCreateImage`, the backend's capture API), so
//! the model-facing screenshot never contains the overlay.
//!
//! The helper runs a manual event loop instead of `NSApplication::run`: every
//! event is inspected before dispatch, which is how a Stop click is detected
//! without defining an Objective-C subclass (and therefore without any
//! `unsafe` code). A Stop click whose Quartz event carries
//! [`SYNTHETIC_INPUT_TAG`] was posted by Haider itself and is ignored.

use super::{
    PresenceCommand, PresenceEvent, PresenceMark, PresencePoint, SYNTHETIC_INPUT_TAG, art,
    encode_event, parse_command_line,
};
use objc2::rc::Retained;
use objc2::{AnyThread as _, MainThreadMarker, MainThreadOnly as _};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSColor, NSEventMask,
    NSEventType, NSFont, NSImage, NSImageView, NSPanel, NSScreen, NSTextAlignment, NSTextField,
    NSWindowCollectionBehavior, NSWindowSharingType, NSWindowStyleMask,
};
use objc2_core_graphics::{CGEvent, CGEventField};
use objc2_foundation::{NSData, NSDate, NSPoint, NSRect, NSSize, NSString};
use std::collections::VecDeque;
use std::io::{BufRead as _, Write as _};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// `NSScreenSaverWindowLevel`: above normal, floating, modal and pop-up
/// menu windows so the indicator cannot be covered by the app under control.
const OVERLAY_LEVEL: isize = 1000;
const FRAME: f64 = 1.0 / 60.0;
const MOVE_DURATION: Duration = Duration::from_millis(160);
const RING_DURATION: Duration = Duration::from_millis(450);
const CAPTION_HOLD: Duration = Duration::from_millis(1200);
const POINTER_WINDOW: (f64, f64) = (190.0, 40.0);
const CAPTION_FRAME: (f64, f64, f64, f64) = (22.0, 20.0, 120.0, 17.0);
const BADGE_MARGIN: f64 = 10.0;
/// Pointer targets this close to the badge move it to the other edge.
const BADGE_AVOID_MARGIN: f64 = 28.0;

pub(super) fn run() -> i32 {
    let Some(mtm) = MainThreadMarker::new() else {
        eprintln!("haiderd: presence overlay must run on the process main thread");
        return 70;
    };
    let queue = Arc::new(Mutex::new(VecDeque::new()));
    let closed = Arc::new(AtomicBool::new(false));
    spawn_stdin_reader(Arc::clone(&queue), Arc::clone(&closed));

    let app = NSApplication::sharedApplication(mtm);
    // Accessory: no Dock icon, no menu bar, never becomes the active app.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    app.finishLaunching();

    let mut overlay = match Overlay::new(mtm) {
        Ok(overlay) => overlay,
        Err(message) => {
            emit(&PresenceEvent::Error { message });
            return 70;
        }
    };
    emit(&PresenceEvent::Ready {
        platform: "macos".into(),
        capture_excluded: !evidence_capturable(),
    });

    let mode = NSString::from_str("kCFRunLoopDefaultMode");
    loop {
        let mut until = NSDate::dateWithTimeIntervalSinceNow(FRAME);
        while let Some(event) = app.nextEventMatchingMask_untilDate_inMode_dequeue(
            NSEventMask::Any,
            Some(&until),
            &mode,
            true,
        ) {
            if !overlay.consume_stop_click(&event) {
                app.sendEvent(&event);
            }
            until = NSDate::distantPast();
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

/// Evidence-only switch: with `HAIDER_CU_PRESENCE_EVIDENCE_CAPTURABLE=1` the
/// overlay is capturable so a human-view screenshot can document what it looks
/// like. It then also appears in model-facing screenshots, which `Ready`
/// reports honestly as `capture_excluded: false`. Never set in production.
fn evidence_capturable() -> bool {
    std::env::var_os(super::PRESENCE_EVIDENCE_CAPTURABLE_ENV).is_some_and(|value| value == "1")
}

fn spawn_stdin_reader(queue: Arc<Mutex<VecDeque<PresenceCommand>>>, closed: Arc<AtomicBool>) {
    let spawned = std::thread::Builder::new()
        .name("presence-stdin".into())
        .spawn(move || {
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                let Ok(line) = line else { break };
                match parse_command_line(&line) {
                    Some(Ok(command)) => {
                        if let Ok(mut queue) = queue.lock() {
                            queue.push_back(command);
                        }
                    }
                    Some(Err(message)) => emit(&PresenceEvent::Error {
                        message: format!("ignored malformed presence command: {message}"),
                    }),
                    None => {}
                }
            }
            closed.store(true, Ordering::Release);
        });
    if spawned.is_err() {
        eprintln!("haiderd: presence overlay could not start its input reader");
        std::process::exit(70);
    }
}

fn emit(event: &PresenceEvent) {
    let mut stdout = std::io::stdout().lock();
    // A closed stdout means the daemon is gone; the stdin reader observes
    // the matching EOF and exits the loop.
    let _ = writeln!(stdout, "{}", encode_event(event));
    let _ = stdout.flush();
}

fn panel(mtm: MainThreadMarker, size: (f64, f64), clickable: bool) -> Retained<NSPanel> {
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(size.0, size.1));
    let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
        NSPanel::alloc(mtm),
        frame,
        NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
        NSBackingStoreType::Buffered,
        false,
    );
    panel.setOpaque(false);
    panel.setHasShadow(false);
    panel.setBackgroundColor(Some(&NSColor::clearColor()));
    panel.setHidesOnDeactivate(false);
    panel.setFloatingPanel(true);
    panel.setBecomesKeyOnlyIfNeeded(true);
    panel.setWorksWhenModal(true);
    // After setFloatingPanel, which resets the level to NSFloatingWindowLevel.
    panel.setLevel(OVERLAY_LEVEL);
    panel.setIgnoresMouseEvents(!clickable);
    // The load-bearing line for the model's view: sharing-none windows are
    // omitted from window-server captures of the display (and from every
    // other capture, screenshots and recordings included).
    panel.setSharingType(if evidence_capturable() {
        NSWindowSharingType::ReadOnly
    } else {
        NSWindowSharingType::None
    });
    panel.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    panel
}

fn image_view(
    mtm: MainThreadMarker,
    bitmap: &art::Bitmap,
    frame: NSRect,
) -> Result<Retained<NSImageView>, String> {
    let png = bitmap.to_png()?;
    let data = NSData::with_bytes(&png);
    let image = NSImage::initWithData(NSImage::alloc(), &data)
        .ok_or_else(|| "AppKit rejected the overlay image".to_owned())?;
    // Hi-DPI: the bitmap has `scale` pixels per point; show it at point size.
    image.setSize(frame.size);
    let view = NSImageView::imageViewWithImage(&image, mtm);
    view.setFrame(frame);
    Ok(view)
}

fn label(
    mtm: MainThreadMarker,
    text: &str,
    frame: NSRect,
    size: f64,
    bold: bool,
    color: &NSColor,
) -> Retained<NSTextField> {
    let field = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    field.setFrame(frame);
    field.setTextColor(Some(color));
    let font = if bold {
        NSFont::boldSystemFontOfSize(size)
    } else {
        NSFont::systemFontOfSize(size)
    };
    field.setFont(Some(&font));
    field
}

fn srgb(color: [u8; 4]) -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(
        f64::from(color[0]) / 255.0,
        f64::from(color[1]) / 255.0,
        f64::from(color[2]) / 255.0,
        f64::from(color[3]) / 255.0,
    )
}

struct Overlay {
    mtm: MainThreadMarker,
    pointer: Retained<NSPanel>,
    caption: Retained<NSTextField>,
    ring: Retained<NSPanel>,
    badge: Retained<NSPanel>,
    badge_text: Retained<NSTextField>,
    stop_text: Retained<NSTextField>,
    visible: bool,
    stopping: bool,
    badge_at_top: bool,
    position: Option<(f64, f64)>,
    move_from: (f64, f64),
    move_to: (f64, f64),
    move_started: Option<Instant>,
    ack_on_arrival: Option<u64>,
    ring_on_arrival: bool,
    ring_started: Option<Instant>,
    caption_until: Option<Instant>,
}

impl Overlay {
    fn new(mtm: MainThreadMarker) -> Result<Self, String> {
        let scale = NSScreen::mainScreen(mtm).map_or(2.0, |screen| screen.backingScaleFactor());

        let pointer = panel(mtm, POINTER_WINDOW, false);
        let pointer_content = pointer
            .contentView()
            .ok_or_else(|| "pointer panel has no content view".to_owned())?;
        let arrow = image_view(
            mtm,
            &art::pointer(scale),
            NSRect::new(
                NSPoint::new(0.0, POINTER_WINDOW.1 - art::POINTER_SIZE.1),
                NSSize::new(art::POINTER_SIZE.0, art::POINTER_SIZE.1),
            ),
        )?;
        pointer_content.addSubview(&arrow);
        let caption = label(
            mtm,
            PresenceMark::Move.caption(),
            NSRect::new(
                NSPoint::new(
                    CAPTION_FRAME.0,
                    POINTER_WINDOW.1 - CAPTION_FRAME.1 - CAPTION_FRAME.3,
                ),
                NSSize::new(CAPTION_FRAME.2, CAPTION_FRAME.3),
            ),
            11.0,
            true,
            &srgb(art::INK),
        );
        caption.setDrawsBackground(true);
        caption.setBackgroundColor(Some(&srgb(art::POINTER_FILL)));
        caption.setAlignment(NSTextAlignment::Center);
        pointer_content.addSubview(&caption);

        let ring = panel(mtm, (art::RING_SIZE, art::RING_SIZE), false);
        let ring_content = ring
            .contentView()
            .ok_or_else(|| "ring panel has no content view".to_owned())?;
        ring_content.addSubview(&*image_view(
            mtm,
            &art::click_ring(scale),
            NSRect::new(
                NSPoint::new(0.0, 0.0),
                NSSize::new(art::RING_SIZE, art::RING_SIZE),
            ),
        )?);

        let badge = panel(mtm, art::BADGE_SIZE, true);
        let badge_content = badge
            .contentView()
            .ok_or_else(|| "badge panel has no content view".to_owned())?;
        badge_content.addSubview(&*image_view(
            mtm,
            &art::badge(scale),
            NSRect::new(
                NSPoint::new(0.0, 0.0),
                NSSize::new(art::BADGE_SIZE.0, art::BADGE_SIZE.1),
            ),
        )?);
        let white = NSColor::whiteColor();
        let badge_text = label(
            mtm,
            "Haider is controlling this screen",
            NSRect::new(NSPoint::new(32.0, 9.0), NSSize::new(262.0, 18.0)),
            13.0,
            true,
            &white,
        );
        badge_content.addSubview(&badge_text);
        let (stop_x, stop_y, stop_w, stop_h) = art::BADGE_STOP_RECT;
        let stop_text = label(
            mtm,
            "Stop",
            NSRect::new(
                NSPoint::new(stop_x, art::BADGE_SIZE.1 - stop_y - stop_h + 4.0),
                NSSize::new(stop_w, 18.0),
            ),
            13.0,
            true,
            &white,
        );
        stop_text.setAlignment(NSTextAlignment::Center);
        badge_content.addSubview(&stop_text);

        Ok(Self {
            mtm,
            pointer,
            caption,
            ring,
            badge,
            badge_text,
            stop_text,
            visible: false,
            stopping: false,
            badge_at_top: true,
            position: None,
            move_from: (0.0, 0.0),
            move_to: (0.0, 0.0),
            move_started: None,
            ack_on_arrival: None,
            ring_on_arrival: false,
            ring_started: None,
            caption_until: None,
        })
    }

    /// Height of the primary display, which anchors both Quartz (top-left)
    /// and Cocoa (bottom-left) global coordinates.
    fn primary_height(&self) -> f64 {
        NSScreen::screens(self.mtm)
            .firstObject()
            .map_or(0.0, |screen| screen.frame().size.height)
    }

    fn badge_frame(&self, top: bool) -> NSRect {
        let visible = NSScreen::mainScreen(self.mtm).map_or(
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1440.0, 900.0)),
            |screen| screen.visibleFrame(),
        );
        let (width, height) = art::BADGE_SIZE;
        let x = visible.origin.x + (visible.size.width - width) / 2.0;
        let y = if top {
            visible.origin.y + visible.size.height - height - BADGE_MARGIN
        } else {
            visible.origin.y + BADGE_MARGIN
        };
        NSRect::new(NSPoint::new(x, y), NSSize::new(width, height))
    }

    /// Converts a Quartz global point (top-left origin) to Cocoa.
    fn cocoa(&self, point: (f64, f64)) -> (f64, f64) {
        (point.0, self.primary_height() - point.1)
    }

    fn apply(&mut self, command: PresenceCommand) {
        match command {
            PresenceCommand::Show { label, .. } => {
                self.badge_text.setStringValue(&NSString::from_str(&label));
                self.stop_text.setStringValue(&NSString::from_str("Stop"));
                self.stopping = false;
                self.visible = true;
                self.badge
                    .setFrame_display(self.badge_frame(self.badge_at_top), true);
                self.badge.orderFrontRegardless();
                if self.position.is_some() {
                    self.pointer.orderFrontRegardless();
                }
            }
            PresenceCommand::Pointer {
                seq, mark, point, ..
            } => self.pointer_to(seq, mark, point),
            PresenceCommand::Stopping { .. } => {
                self.stopping = true;
                self.stop_text.setStringValue(&NSString::from_str("…"));
            }
            PresenceCommand::Hide { .. } => self.hide(),
        }
    }

    fn pointer_to(&mut self, seq: u64, mark: PresenceMark, point: Option<PresencePoint>) {
        self.caption
            .setStringValue(&NSString::from_str(mark.caption()));
        self.caption_until = Some(Instant::now() + CAPTION_HOLD);
        let target = point.map(|point| (point.x, point.y)).or(self.position);
        let Some(target) = target else {
            // No coordinate yet (typing before any pointer action): badge only.
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
        if self.visible {
            self.pointer.orderFrontRegardless();
        }
        if mark.posts_pointer_input() && from != target {
            // Acknowledge when the pointer visibly arrives, so the real
            // click lands under the drawn arrow (daemon waits ≤250 ms).
            if let Some(previous) = self.ack_on_arrival.replace(seq) {
                emit(&PresenceEvent::Ack { seq: previous });
            }
        } else {
            emit(&PresenceEvent::Ack { seq });
        }
        self.position = Some(target);
        self.animate(Instant::now());
    }

    /// Moves the clickable badge to the opposite screen edge when a
    /// synthetic click would land on (or near) it.
    fn avoid_badge(&mut self, quartz: (f64, f64)) {
        let (x, y) = self.cocoa(quartz);
        let frame = self.badge_frame(self.badge_at_top);
        let near = x >= frame.origin.x - BADGE_AVOID_MARGIN
            && x <= frame.origin.x + frame.size.width + BADGE_AVOID_MARGIN
            && y >= frame.origin.y - BADGE_AVOID_MARGIN
            && y <= frame.origin.y + frame.size.height + BADGE_AVOID_MARGIN;
        if near {
            self.badge_at_top = !self.badge_at_top;
            self.badge
                .setFrame_display(self.badge_frame(self.badge_at_top), true);
        }
    }

    fn place_pointer(&self, quartz: (f64, f64)) {
        let (x, y) = self.cocoa(quartz);
        let origin = NSPoint::new(
            x - art::POINTER_TIP,
            y - (POINTER_WINDOW.1 - art::POINTER_TIP),
        );
        self.pointer.setFrameOrigin(origin);
    }

    fn animate(&mut self, now: Instant) {
        if let Some(started) = self.move_started {
            let progress = (now.saturating_duration_since(started).as_secs_f64()
                / MOVE_DURATION.as_secs_f64())
            .min(1.0);
            let eased = 1.0 - (1.0 - progress).powi(3);
            let current = (
                self.move_from.0 + (self.move_to.0 - self.move_from.0) * eased,
                self.move_from.1 + (self.move_to.1 - self.move_from.1) * eased,
            );
            self.place_pointer(current);
            if progress >= 1.0 {
                self.move_started = None;
                if let Some(seq) = self.ack_on_arrival.take() {
                    emit(&PresenceEvent::Ack { seq });
                }
                if std::mem::take(&mut self.ring_on_arrival) && self.visible {
                    let (x, y) = self.cocoa(self.move_to);
                    let half = art::RING_SIZE / 2.0;
                    self.ring.setFrameOrigin(NSPoint::new(x - half, y - half));
                    self.ring.setAlphaValue(1.0);
                    self.ring.orderFrontRegardless();
                    self.ring_started = Some(now);
                }
            }
        }
        if let Some(started) = self.ring_started {
            let progress =
                now.saturating_duration_since(started).as_secs_f64() / RING_DURATION.as_secs_f64();
            if progress >= 1.0 {
                self.ring.orderOut(None);
                self.ring_started = None;
            } else {
                self.ring.setAlphaValue(1.0 - progress);
            }
        }
        if self.caption_until.is_some_and(|until| now >= until) {
            self.caption_until = None;
            self.caption
                .setStringValue(&NSString::from_str(PresenceMark::Move.caption()));
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
        self.pointer.orderOut(None);
        self.ring.orderOut(None);
        self.badge.orderOut(None);
    }

    /// Returns true when `event` was a human Stop click (consumed).
    fn consume_stop_click(&mut self, event: &objc2_app_kit::NSEvent) -> bool {
        if event.r#type() != NSEventType::LeftMouseDown
            || !self.visible
            || event.windowNumber() != self.badge.windowNumber()
        {
            return false;
        }
        let location = event.locationInWindow();
        let top_left_y = art::BADGE_SIZE.1 - location.y;
        if !art::badge_stop_hit(location.x, top_left_y) {
            return true;
        }
        let synthetic = event.CGEvent().is_some_and(|quartz| {
            CGEvent::integer_value_field(Some(&quartz), CGEventField::EventSourceUserData)
                == SYNTHETIC_INPUT_TAG
        });
        if synthetic {
            emit(&PresenceEvent::Error {
                message: "ignored a Stop click synthesised by Haider".into(),
            });
            return true;
        }
        if !self.stopping {
            self.stopping = true;
            self.stop_text.setStringValue(&NSString::from_str("…"));
            emit(&PresenceEvent::Stop);
        }
        true
    }
}
