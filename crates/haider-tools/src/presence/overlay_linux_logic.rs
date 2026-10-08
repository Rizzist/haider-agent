//! Posting state for the Linux notification helper. The D-Bus loop owns the
//! actual notification and Stop router; this state decides if it may post.

use std::future::Future;
use std::time::Duration;

/// A close that never receives the notification server's reply cannot
/// authorize an image capture. The caller emits the typed failure on Err.
pub(super) async fn confirm_close<F>(close: F, deadline: Duration) -> Result<(), String>
where
    F: Future<Output = Result<(), String>>,
{
    tokio::time::timeout(deadline, close)
        .await
        .map_err(|_| "desktop notification close timed out".to_owned())?
}

#[derive(Default)]
pub(super) struct LinuxPopupState {
    concealed: bool,
    last_body: String,
}

impl LinuxPopupState {
    pub(super) fn show(&mut self, body: &str) -> bool {
        self.last_body = body.to_owned();
        !self.concealed
    }

    pub(super) fn pointer(&mut self, body: &str, due: bool) -> bool {
        self.last_body = body.to_owned();
        due && !self.concealed
    }

    pub(super) fn conceal(&mut self) {
        self.concealed = true;
    }

    pub(super) fn reveal(&mut self, active: bool, stopping: bool) -> Option<String> {
        if !std::mem::take(&mut self.concealed) || !active || stopping {
            return None;
        }
        Some(self.last_body.clone())
    }

    pub(super) fn hide(&mut self) {
        self.concealed = false;
        self.last_body.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn close_timeout_and_error_fail_closed() {
        assert!(
            confirm_close(async { Ok(()) }, Duration::from_millis(5))
                .await
                .is_ok()
        );
        assert!(
            confirm_close(
                async { Err("server refused".into()) },
                Duration::from_millis(5)
            )
            .await
            .is_err()
        );
        let timed_out = confirm_close(std::future::pending(), Duration::from_millis(5)).await;
        assert_eq!(
            timed_out,
            Err("desktop notification close timed out".into())
        );
    }

    #[test]
    fn cross_session_updates_are_recorded_but_never_posted_during_conceal() {
        let mut popup = LinuxPopupState::default();
        assert!(popup.show("initial"));
        assert!(popup.pointer("click", true));
        popup.conceal();
        assert!(!popup.show("new session"));
        assert!(!popup.pointer("wait", true));
        assert!(!popup.pointer("type", false));
        assert!(!popup.pointer("key", true));
        assert!(!popup.pointer("click at (1, 2)", true));
        assert_eq!(
            popup.reveal(true, false).as_deref(),
            Some("click at (1, 2)")
        );
        assert!(popup.pointer("after reveal", true));
        assert_eq!(
            popup.reveal(true, false),
            None,
            "duplicate reveal never reposts"
        );
    }

    #[test]
    fn hide_or_stop_during_conceal_has_no_notification_to_restore() {
        let mut popup = LinuxPopupState::default();
        popup.show("initial");
        popup.conceal();
        popup.hide();
        assert!(
            popup.show("new generation"),
            "Hide resets conceal for a later run"
        );
        assert_eq!(popup.reveal(false, false), None);
        popup.conceal();
        assert_eq!(popup.reveal(true, true), None);
    }

    #[test]
    fn concealed_label_and_body_are_preserved_verbatim() {
        let bodies = [
            String::new(),
            "  mixed CASE / stop \\ \n".to_owned(),
            "مرحبا · café · 🔴".to_owned(),
            "x".repeat(8192),
        ];
        for body in bodies {
            let mut popup = LinuxPopupState::default();
            popup.conceal();
            assert!(!popup.show(&body));
            assert!(!popup.pointer(&body, true));
            assert_eq!(popup.reveal(true, false).as_deref(), Some(body.as_str()));
        }
    }
}
