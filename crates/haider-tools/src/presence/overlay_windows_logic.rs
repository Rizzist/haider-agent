//! Platform-independent state of the Windows presence overlay.
//!
//! The Win32 surfaces live in `overlay_windows.rs` (`win::Layered`); this
//! module owns every decision about when a window is shown or hidden, so the
//! decisions compile — and are tested — on every platform with fake windows.
//!
//! Visibility rule: the pointer, ring and badge are only ever shown while the
//! overlay is `visible` AND not `concealed`. `Conceal` (sent by the daemon
//! around each model-facing capture when capture exclusion was refused) sets
//! `concealed`; only `Reveal` clears it. Animation frames, `Show`, badge
//! relocation and ring flashes keep updating their state while concealed but
//! never show a window (verifier finding B1 on 8c614f6c: the next animation
//! frame used to re-show the pointer mid-capture).

use super::{PresenceCommand, PresenceEvent, PresenceMark, PresencePoint, art};
use std::time::{Duration, Instant};

pub(super) const MOVE_DURATION: Duration = Duration::from_millis(160);
pub(super) const RING_DURATION: Duration = Duration::from_millis(450);
const BADGE_MARGIN: f64 = 10.0;
const BADGE_AVOID_MARGIN: f64 = 28.0;

/// Badge text run: text, rectangle `(left, top, right, bottom)`, centred.
pub(super) type BadgeText<'a> = (&'a str, (f64, f64, f64, f64), bool);

/// One overlay surface (a layered Win32 popup in production).
pub(super) trait OverlayWindow {
    fn show_at(&self, x: i32, y: i32);
    fn hide(&self);
    fn set_image(&self, bitmap: &art::Bitmap, alpha: u8) -> Result<(), String>;
    fn set_image_with_text(
        &self,
        bitmap: &art::Bitmap,
        texts: &[BadgeText<'_>],
        font_px: i32,
    ) -> Result<(), String>;
}

pub(super) struct Overlay<W: OverlayWindow> {
    scale: f64,
    /// Primary work area `(left, top, right, bottom)` in physical pixels.
    work_area: fn() -> (i32, i32, i32, i32),
    pointer: W,
    ring: W,
    badge: W,
    ring_bitmap: art::Bitmap,
    pub(super) capture_excluded: bool,
    pub(super) visible: bool,
    pub(super) stopping: bool,
    /// Between `Conceal` and `Reveal`: nothing may be shown.
    concealed: bool,
    badge_at_top: bool,
    label: String,
    position: Option<(f64, f64)>,
    move_from: (f64, f64),
    move_to: (f64, f64),
    move_started: Option<Instant>,
    ack_on_arrival: Option<u64>,
    ring_on_arrival: bool,
    ring_started: Option<Instant>,
    /// Events for the daemon, drained by the run loop after every step.
    outbox: Vec<PresenceEvent>,
}

impl<W: OverlayWindow> Overlay<W> {
    pub(super) fn new(
        scale: f64,
        work_area: fn() -> (i32, i32, i32, i32),
        pointer: W,
        ring: W,
        badge: W,
        capture_excluded: bool,
    ) -> Result<Self, String> {
        pointer.set_image(&art::pointer(scale), 255)?;
        let ring_bitmap = art::click_ring(scale);
        ring.set_image(&ring_bitmap, 255)?;
        let mut overlay = Self {
            scale,
            work_area,
            pointer,
            ring,
            badge,
            ring_bitmap,
            capture_excluded,
            visible: false,
            stopping: false,
            concealed: false,
            badge_at_top: true,
            label: "Haider is controlling this screen".into(),
            position: None,
            move_from: (0.0, 0.0),
            move_to: (0.0, 0.0),
            move_started: None,
            ack_on_arrival: None,
            ring_on_arrival: false,
            ring_started: None,
            outbox: Vec::new(),
        };
        overlay.draw_badge("Stop")?;
        Ok(overlay)
    }

    /// Events produced since the last call.
    pub(super) fn take_events(&mut self) -> Vec<PresenceEvent> {
        std::mem::take(&mut self.outbox)
    }

    fn emit(&mut self, event: PresenceEvent) {
        self.outbox.push(event);
    }

    /// The single visibility gate for every window.
    fn may_show(&self) -> bool {
        self.visible && !self.concealed
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
        let work = (self.work_area)();
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

    fn show_badge(&self) {
        if self.may_show() {
            let (x, y) = self.badge_origin(self.badge_at_top);
            self.badge.show_at(x, y);
        }
    }

    pub(super) fn apply(&mut self, command: PresenceCommand, now: Instant) {
        match command {
            PresenceCommand::Show { label, .. } => {
                self.label = label;
                self.stopping = false;
                let _ = self.draw_badge("Stop");
                self.visible = true;
                self.show_badge();
                if let Some(position) = self.position {
                    self.place_pointer(position);
                }
            }
            PresenceCommand::Pointer {
                seq, mark, point, ..
            } => self.pointer_to(seq, mark, point, now),
            PresenceCommand::Stopping { .. } => {
                self.stopping = true;
                let _ = self.draw_badge("…");
            }
            PresenceCommand::Hide { .. } => self.hide(),
            // The daemon also conceals before it has read `Ready`; windows
            // with WDA_EXCLUDEFROMCAPTURE are already absent from captures,
            // so only the refused-exclusion fallback takes them down.
            PresenceCommand::Conceal { seq, .. } => {
                if !self.capture_excluded {
                    self.concealed = true;
                    self.pointer.hide();
                    self.ring.hide();
                    self.badge.hide();
                }
                self.emit(PresenceEvent::Ack { seq });
            }
            PresenceCommand::Reveal { .. } => {
                if self.concealed {
                    self.concealed = false;
                    self.show_badge();
                    if let Some(position) = self.position {
                        self.place_pointer(position);
                    }
                }
            }
        }
    }

    fn pointer_to(
        &mut self,
        seq: u64,
        mark: PresenceMark,
        point: Option<PresencePoint>,
        now: Instant,
    ) {
        let Some(target) = point.map(|point| (point.x, point.y)).or(self.position) else {
            self.emit(PresenceEvent::Ack { seq });
            return;
        };
        if mark.posts_pointer_input() {
            self.avoid_badge(target);
        }
        let from = self.position.unwrap_or(target);
        self.move_from = from;
        self.move_to = target;
        self.move_started = Some(now);
        self.ring_on_arrival = mark.posts_pointer_input();
        if mark.posts_pointer_input() && from != target {
            if let Some(previous) = self.ack_on_arrival.replace(seq) {
                self.emit(PresenceEvent::Ack { seq: previous });
            }
        } else {
            self.emit(PresenceEvent::Ack { seq });
        }
        self.position = Some(target);
        self.animate(now);
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
            self.show_badge();
        }
    }

    fn place_pointer(&self, point: (f64, f64)) {
        if !self.may_show() {
            return;
        }
        let tip = art::POINTER_TIP * self.scale;
        self.pointer.show_at(
            (point.0 - tip).round() as i32,
            (point.1 - tip).round() as i32,
        );
    }

    pub(super) fn animate(&mut self, now: Instant) {
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
                    self.emit(PresenceEvent::Ack { seq });
                }
                if std::mem::take(&mut self.ring_on_arrival) && self.may_show() {
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
                // A layered window's image can change while hidden; this
                // never shows it.
                let alpha = ((1.0 - progress) * 255.0).round() as u8;
                let _ = self.ring.set_image(&self.ring_bitmap, alpha);
            }
        }
    }

    pub(super) fn hide(&mut self) {
        self.visible = false;
        self.stopping = false;
        self.move_started = None;
        self.ring_started = None;
        if let Some(seq) = self.ack_on_arrival.take() {
            self.emit(PresenceEvent::Ack { seq });
        }
        self.pointer.hide();
        self.ring.hide();
        self.badge.hide();
    }
}

#[cfg(test)]
#[path = "overlay_windows_logic_tests.rs"]
mod tests;
