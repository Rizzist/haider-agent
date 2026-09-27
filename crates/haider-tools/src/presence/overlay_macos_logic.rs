//! Visibility state shared by the macOS panels and cross-platform tests.

use super::PresenceEvent;

#[derive(Default)]
pub(super) struct MacVisibility {
    pub(super) visible: bool,
    concealed: bool,
}

/// The capture boundary may acknowledge only when none of its three panel
/// window numbers appears in the window server's on-screen list.
pub(super) fn panels_absent_in_window_list(panels: &[u32; 3], on_screen: &[u32]) -> bool {
    panels.iter().all(|number| !on_screen.contains(number))
}

/// Keep the panel hide, window-server verification and acknowledgement in
/// one ordered operation. An unverified hide is a typed failure, never Ack.
pub(super) fn conceal_after_hide<T>(
    seq: u64,
    state: &mut T,
    hide: impl FnOnce(&mut T),
    verify: impl FnOnce(&T) -> Result<(), String>,
) -> PresenceEvent {
    hide(state);
    match verify(state) {
        Ok(()) => PresenceEvent::Ack { seq },
        Err(message) => PresenceEvent::ConcealFailed { seq, message },
    }
}

impl MacVisibility {
    pub(super) fn show(&mut self) -> bool {
        self.visible = true;
        self.may_show()
    }

    pub(super) fn may_show(&self) -> bool {
        self.visible && !self.concealed
    }

    pub(super) fn conceal(&mut self) {
        self.concealed = true;
    }

    pub(super) fn reveal(&mut self) -> bool {
        std::mem::take(&mut self.concealed) && self.visible
    }

    pub(super) fn hide(&mut self) {
        self.visible = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn conceal_acks_only_after_hide_and_verification() {
        #[derive(Default)]
        struct Fake {
            hidden: bool,
            verified: Cell<bool>,
        }
        let mut fake = Fake::default();
        let event = conceal_after_hide(
            41,
            &mut fake,
            |fake| fake.hidden = true,
            |fake| {
                assert!(fake.hidden, "all panels must be hidden before verification");
                fake.verified.set(true);
                Ok(())
            },
        );
        assert!(fake.verified.get(), "Ack requires the verifier to run");
        assert_eq!(event, PresenceEvent::Ack { seq: 41 });
    }

    #[test]
    fn conceal_verification_timeout_fails_closed() {
        let mut hidden = false;
        let event = conceal_after_hide(
            42,
            &mut hidden,
            |hidden| *hidden = true,
            |hidden| {
                assert!(*hidden);
                Err("display refresh deadline elapsed".into())
            },
        );
        assert!(hidden);
        assert_eq!(
            event,
            PresenceEvent::ConcealFailed {
                seq: 42,
                message: "display refresh deadline elapsed".into(),
            }
        );
    }

    #[test]
    fn conceal_window_list_requires_pointer_ring_and_badge_absent() {
        let panels = [11, 12, 13];
        for present in panels {
            assert!(!panels_absent_in_window_list(&panels, &[99, present]));
        }
        assert!(panels_absent_in_window_list(&panels, &[98, 99]));
        assert!(panels_absent_in_window_list(&panels, &[]));
    }

    #[test]
    fn show_pointer_animation_and_ring_stay_hidden_until_reveal() {
        let mut panels = MacVisibility::default();
        assert!(panels.show(), "normal Show remains visible");
        panels.conceal();
        assert!(
            panels.visible,
            "a queued Stop click still belongs to the run"
        );
        assert!(
            !panels.show(),
            "cross-session Show cannot order a panel front"
        );
        for _ in 0..40 {
            assert!(!panels.may_show(), "pointer, ring and frame gate");
        }
        assert!(
            panels.reveal(),
            "latest label and pointer may be ordered front"
        );
        assert!(panels.may_show());
        panels.conceal();
        panels.hide();
        assert!(!panels.reveal(), "Hide leaves no panel to restore");
    }
}
