//! W-C M2 — desktop notifications: trigger states (terminal + park, never
//! mid-stream), the focus gate (both branches), masked text, the toggle,
//! debounce (one per turn), and the non-tty OSC suppression.
#![allow(clippy::expect_used)]

use haider_protocol::ids::{MenuId, SessionId};
use haider_protocol::state::{RunState, WaitReason};
use haider_tui::app::{AppModel, RuntimeMode, Screen};
use haider_tui::notify::{self, Attention};

fn attached_model() -> AppModel {
    let mut model = AppModel::new();
    model.mode = RuntimeMode::Live;
    model.screen = Screen::Session;
    model.active_session = Some(SessionId::new("s-1"));
    // Unfocused with focus reported: the strict gate applies.
    model.set_focus(false);
    model
}

// ---------------------------------------------------------------------------
// Pure notify-module laws
// ---------------------------------------------------------------------------

#[test]
fn attention_fires_on_terminal_and_park_states_only() {
    assert_eq!(
        notify::attention_for(&RunState::Done),
        Some(Attention::Done)
    );
    assert_eq!(
        notify::attention_for(&RunState::Errored),
        Some(Attention::Errored)
    );
    assert_eq!(
        notify::attention_for(&RunState::PermissionRequired {
            menu: MenuId::new("m")
        }),
        Some(Attention::Permission)
    );
    assert_eq!(
        notify::attention_for(&RunState::InputRequired {
            menu: MenuId::new("m")
        }),
        Some(Attention::Input)
    );
    assert_eq!(
        notify::attention_for(&RunState::Waiting {
            reason: WaitReason::DeviceUnreachable
        }),
        Some(Attention::WaitingDevice)
    );
    // Mid-stream states NEVER notify.
    assert_eq!(notify::attention_for(&RunState::Thinking), None);
    assert_eq!(notify::attention_for(&RunState::Streaming), None);
    assert_eq!(notify::attention_for(&RunState::RunningTool), None);
    // A non-device wait is not an attention park.
    assert_eq!(
        notify::attention_for(&RunState::Waiting {
            reason: WaitReason::RateLimit
        }),
        None
    );
}

#[test]
fn masked_text_hides_an_email_via_the_one_authority() {
    let masked = notify::mask_text("ping alice@example.com now");
    assert!(
        !masked.contains("alice@example.com"),
        "raw email survived: {masked}"
    );
    assert_eq!(
        masked,
        format!(
            "ping {} now",
            haider_tui::format::mask_identity("alice@example.com")
        )
    );
}

#[test]
fn osc9_wraps_and_strips_control_bytes() {
    // A stray BEL/ESC in the text can never terminate or inject the sequence.
    let seq = notify::osc9("done\u{7}\u{1b}[31m here");
    assert!(seq.starts_with("\u{1b}]9;"), "OSC 9 prefix: {seq:?}");
    assert!(seq.ends_with('\u{7}'), "BEL terminator: {seq:?}");
    // Exactly ONE BEL — the terminator; the embedded one was stripped.
    assert_eq!(seq.matches('\u{7}').count(), 1);
    assert!(!seq.contains("\u{1b}["), "embedded ESC stripped: {seq:?}");
}

#[test]
fn non_tty_sink_emits_no_osc_bytes() {
    let line = "haider: turn done";
    // A tty gets the sequence; a pipe/redirect gets NOTHING.
    assert!(!notify::osc9_for_tty(line, true).is_empty());
    assert_eq!(notify::osc9_for_tty(line, false), Vec::<u8>::new());
}

// ---------------------------------------------------------------------------
// App firing laws
// ---------------------------------------------------------------------------

#[test]
fn fires_on_a_terminal_transition_when_unfocused() {
    let mut model = attached_model();
    // A mid-stream state queues nothing.
    model.note_run_state_for_notifications(&RunState::Streaming);
    assert!(model.notifications.is_empty(), "no mid-stream ping");
    // Reaching Done fires exactly one masked line.
    model.note_run_state_for_notifications(&RunState::Done);
    assert_eq!(model.notifications.len(), 1);
    assert!(
        model.notifications[0].contains("turn done"),
        "{:?}",
        model.notifications
    );
}

#[test]
fn fires_on_a_permission_park() {
    let mut model = attached_model();
    model.note_run_state_for_notifications(&RunState::Streaming);
    model.note_run_state_for_notifications(&RunState::PermissionRequired {
        menu: MenuId::new("m"),
    });
    assert_eq!(model.notifications.len(), 1);
    assert!(
        model.notifications[0].contains("approval"),
        "{:?}",
        model.notifications
    );
}

#[test]
fn focus_gate_suppresses_when_focused_but_fires_when_focus_unreported() {
    // Focused (and reported) → silent.
    let mut focused = AppModel::new();
    focused.mode = RuntimeMode::Live;
    focused.set_focus(true);
    focused.note_run_state_for_notifications(&RunState::Done);
    assert!(
        focused.notifications.is_empty(),
        "focused terminal stays silent"
    );

    // Focus NEVER reported → fire anyway (redundant ping beats a missed one).
    let mut unknown = AppModel::new();
    unknown.mode = RuntimeMode::Live;
    assert!(!unknown.focus_reported, "focus starts unreported");
    unknown.note_run_state_for_notifications(&RunState::Done);
    assert_eq!(unknown.notifications.len(), 1, "fallback fires");
}

#[test]
fn masked_title_never_leaks_a_secret() {
    let mut model = attached_model();
    model.session_title = Some("deploy for alice@corp.example".to_owned());
    model.note_run_state_for_notifications(&RunState::Done);
    let line = model.notifications.first().cloned().unwrap_or_default();
    assert!(
        !line.contains("alice@corp.example"),
        "raw identity leaked: {line}"
    );
    assert!(
        line.contains("turn done"),
        "still names the outcome: {line}"
    );
}

#[test]
fn toggle_off_is_silent() {
    let mut model = attached_model();
    model.toggle_notifications(Some(false));
    model.note_run_state_for_notifications(&RunState::Done);
    assert!(model.notifications.is_empty(), "toggle off → no ping");
    // Back on and it fires again (a fresh turn's transition).
    model.toggle_notifications(Some(true));
    model.note_run_state_for_notifications(&RunState::Streaming);
    model.note_run_state_for_notifications(&RunState::Done);
    assert_eq!(model.notifications.len(), 1);
}

#[test]
fn debounce_one_notification_per_turn() {
    let mut model = attached_model();
    model.note_run_state_for_notifications(&RunState::Done);
    // A replay of the SAME terminal state does not re-notify.
    model.note_run_state_for_notifications(&RunState::Done);
    model.note_run_state_for_notifications(&RunState::Done);
    assert_eq!(
        model.notifications.len(),
        1,
        "one per turn: {:?}",
        model.notifications
    );
}

#[test]
fn settings_persist_the_toggle_and_preserve_the_theme() {
    use haider_tui::settings::SettingsStore;
    use haider_tui::theme::ThemeChoice;
    let temp = tempfile::tempdir().expect("temp");
    let path = temp.path().join("tui-settings.json");
    let dark = ThemeChoice::parse("dark").expect("dark theme");

    let mut store = SettingsStore::at(path.clone());
    // Absent file → notifications default ON.
    assert!(store.load_notifications(), "default on");
    // Commit a theme (notifications ride along as the tracked default)...
    store.save_if_changed(dark);
    // ...then turn notifications OFF — the theme must survive the write.
    store.save_notifications_if_changed(dark, false);

    let reopened = SettingsStore::at(path);
    assert!(!reopened.load_notifications(), "off persisted");
    assert_eq!(
        reopened.load(),
        Some(dark),
        "theme preserved across the toggle write"
    );
}
