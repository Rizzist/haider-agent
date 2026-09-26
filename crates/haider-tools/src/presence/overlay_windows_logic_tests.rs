#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;
use crate::presence::{PresenceEndReason, PresenceSurface};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// A fake layered window: records visibility and every show.
#[derive(Clone, Default)]
struct FakeWindow {
    name: &'static str,
    visible: Rc<Cell<bool>>,
    shows: Rc<RefCell<Vec<&'static str>>>,
}

impl FakeWindow {
    fn new(name: &'static str, shows: &Rc<RefCell<Vec<&'static str>>>) -> Self {
        Self {
            name,
            visible: Rc::default(),
            shows: Rc::clone(shows),
        }
    }
}

impl OverlayWindow for FakeWindow {
    fn show_at(&self, _x: i32, _y: i32) {
        self.visible.set(true);
        self.shows.borrow_mut().push(self.name);
    }
    fn hide(&self) {
        self.visible.set(false);
    }
    fn set_image(&self, _bitmap: &art::Bitmap, _alpha: u8) -> Result<(), String> {
        Ok(())
    }
    fn set_image_with_text(
        &self,
        _bitmap: &art::Bitmap,
        _texts: &[BadgeText<'_>],
        _font_px: i32,
    ) -> Result<(), String> {
        Ok(())
    }
}

fn work_area() -> (i32, i32, i32, i32) {
    (0, 0, 1920, 1040)
}

struct Harness {
    overlay: Overlay<FakeWindow>,
    pointer: FakeWindow,
    ring: FakeWindow,
    badge: FakeWindow,
    shows: Rc<RefCell<Vec<&'static str>>>,
}

fn harness(capture_excluded: bool) -> Harness {
    let shows = Rc::new(RefCell::new(Vec::new()));
    let (pointer, ring, badge) = (
        FakeWindow::new("pointer", &shows),
        FakeWindow::new("ring", &shows),
        FakeWindow::new("badge", &shows),
    );
    let overlay = Overlay::new(
        1.0,
        work_area,
        pointer.clone(),
        ring.clone(),
        badge.clone(),
        capture_excluded,
    )
    .expect("overlay");
    Harness {
        overlay,
        pointer,
        ring,
        badge,
        shows,
    }
}

fn pointer(seq: u64, mark: PresenceMark, point: Option<(f64, f64)>) -> PresenceCommand {
    PresenceCommand::Pointer {
        surface: PresenceSurface::Screen,
        seq,
        mark,
        point: point.map(|(x, y)| PresencePoint { x, y }),
    }
}

fn anything_visible(h: &Harness) -> bool {
    h.pointer.visible.get() || h.ring.visible.get() || h.badge.visible.get()
}

/// Verifier finding B1 (8c614f6c): on the refused-exclusion fallback a
/// Conceal hid the windows, but the very next animation frame re-showed the
/// pointer (SWP_SHOWWINDOW) mid-capture. The normal sequence is move →
/// screenshot (Observe pointer, no point) → Conceal → frame.
/// MUTATION CHECK: drop `concealed` from `may_show`. Expected runtime
/// failure: a window is visible after the concealed frames.
#[test]
fn conceal_holds_across_animation_frames_until_reveal() {
    let mut h = harness(false);
    let t0 = Instant::now();
    h.overlay.apply(
        PresenceCommand::Show {
            surface: PresenceSurface::Screen,
            label: "Haider is controlling this screen".into(),
        },
        t0,
    );
    // A click far from the badge, run to completion (ring flashes).
    h.overlay
        .apply(pointer(1, PresenceMark::Click, Some((900.0, 700.0))), t0);
    h.overlay.animate(t0 + MOVE_DURATION);
    assert!(h.pointer.visible.get() && h.badge.visible.get());
    // The model's next action is a screenshot: Observe with no coordinate
    // schedules a (zero-length) animation, then the daemon conceals.
    let t1 = t0 + MOVE_DURATION + Duration::from_millis(5);
    h.overlay.apply(pointer(2, PresenceMark::Observe, None), t1);
    h.overlay.apply(
        PresenceCommand::Conceal {
            surface: PresenceSurface::Screen,
            seq: 99,
        },
        t1,
    );
    assert!(!anything_visible(&h), "Conceal takes everything down");
    let shows_before = h.shows.borrow().len();
    // Frames during the capture: animation, ring fade, even a Show and a
    // badge-relocating click must not re-show anything.
    for frame in 1..=40 {
        h.overlay.animate(t1 + Duration::from_millis(16 * frame));
    }
    h.overlay.apply(
        PresenceCommand::Show {
            surface: PresenceSurface::Screen,
            label: "Haider is controlling this screen".into(),
        },
        t1,
    );
    h.overlay
        .apply(pointer(3, PresenceMark::Click, Some((960.0, 20.0))), t1);
    for frame in 41..=80 {
        h.overlay.animate(t1 + Duration::from_millis(16 * frame));
    }
    assert!(!anything_visible(&h), "nothing re-appears while concealed");
    assert_eq!(h.shows.borrow().len(), shows_before, "no show call at all");
    assert!(
        h.overlay
            .take_events()
            .contains(&PresenceEvent::Ack { seq: 99 }),
        "the conceal is acknowledged"
    );
    // Reveal restores the badge and pointer.
    h.overlay.apply(
        PresenceCommand::Reveal {
            surface: PresenceSurface::Screen,
        },
        t1 + Duration::from_secs(2),
    );
    assert!(h.badge.visible.get() && h.pointer.visible.get());
}

#[test]
fn excluded_windows_ack_conceal_without_hiding() {
    let mut h = harness(true);
    let t0 = Instant::now();
    h.overlay.apply(
        PresenceCommand::Show {
            surface: PresenceSurface::Screen,
            label: "x".into(),
        },
        t0,
    );
    h.overlay.apply(
        PresenceCommand::Conceal {
            surface: PresenceSurface::Screen,
            seq: 7,
        },
        t0,
    );
    assert!(h.badge.visible.get(), "capture-excluded windows stay up");
    assert!(
        h.overlay
            .take_events()
            .contains(&PresenceEvent::Ack { seq: 7 })
    );
    h.overlay.apply(
        PresenceCommand::Hide {
            surface: PresenceSurface::Screen,
            reason: PresenceEndReason::RunEnded,
        },
        t0,
    );
    assert!(!anything_visible(&h));
}
