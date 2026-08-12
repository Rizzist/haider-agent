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
    // A long provider-reset park needs attention; transient Retrying below
    // remains silent.
    assert_eq!(
        notify::attention_for(&RunState::Waiting {
            reason: WaitReason::RateLimit
        }),
        Some(Attention::WaitingRateLimit)
    );
    assert_eq!(
        notify::attention_for(&RunState::EffectOutcomeUnknown),
        Some(Attention::EffectUnknown)
    );
    // M4: a retry backoff is mid-run work, NEVER a terminal/park — it must
    // not fire a desktop notification (only the FINAL Errored does).
    assert_eq!(
        notify::attention_for(&RunState::Retrying {
            attempt: 2,
            max: 10,
            delay_ms: 1_000,
            reason: WaitReason::ProviderBackoff,
        }),
        None
    );
}

#[test]
fn retry_wait_fires_no_notification() {
    // M4 × M2 interplay: a run backing off after a retryable failure queues
    // nothing; only a real terminal (Done/Errored) or a park does.
    let mut model = attached_model();
    model.note_run_state_for_notifications(&RunState::Retrying {
        attempt: 2,
        max: 10,
        delay_ms: 1_000,
        reason: WaitReason::ProviderBackoff,
    });
    assert!(
        model.notifications.is_empty(),
        "a retry wait must stay silent: {:?}",
        model.notifications
    );
    // The FINAL Errored (retries exhausted) still notifies.
    model.note_run_state_for_notifications(&RunState::Errored);
    assert_eq!(model.notifications.len(), 1, "the terminal Errored fires");
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
fn mask_text_hides_api_keys_and_bearer_tokens_not_just_emails() {
    // H3: secret-shaped tokens (API keys, bearer JWTs) are masked too — the
    // old pass masked only `@`-tokens, so an `sk-…` sailed through. Ordinary
    // prose words are still left alone.
    let key = "sk-ant-api03-SEKRET1234567890abcd";
    let masked = notify::mask_text(&format!("deploy {key} for prod"));
    assert!(!masked.contains(key), "sk- key leaked: {masked}");
    assert!(
        masked.contains("deploy") && masked.contains("for") && masked.contains("prod"),
        "prose kept: {masked}"
    );
    // A bearer JWT is masked as well.
    let jwt = "eyJhbGciOiJIUzI1NiJ9.payloadpart123.sigpart456";
    let masked_jwt = notify::mask_text(&format!("token {jwt}"));
    assert!(!masked_jwt.contains(jwt), "jwt leaked: {masked_jwt}");
}

#[test]
fn notification_osc9_bytes_mask_an_api_key_in_the_title() {
    // H3 end-to-end: an `sk-…` in a session title never reaches the OSC 9 bytes
    // (nor the OS notification history) in the clear.
    let mut model = attached_model();
    let secret = "sk-ant-api03-DEADBEEFsecret0001x";
    model.session_title = Some(format!("release {secret}"));
    model.note_run_state_for_notifications(&RunState::Done);
    let line = model.notifications.first().cloned().unwrap_or_default();
    assert!(
        !line.contains(secret),
        "raw key leaked into the line: {line}"
    );
    let bytes = notify::osc9_for_tty(&line, true);
    let emitted = String::from_utf8_lossy(&bytes);
    assert!(
        !emitted.contains(secret),
        "raw key in OSC 9 bytes: {emitted}"
    );
    assert!(
        emitted.contains("turn done"),
        "still names the outcome: {emitted}"
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

// ---------------------------------------------------------------------------
// M10 — background-session terminal notification (route_raw, the real runtime
// entry point for the event stream)
// ---------------------------------------------------------------------------

fn run_state_envelope(
    session: &SessionId,
    seq: u64,
    state: RunState,
) -> haider_protocol::envelope::RawEnvelope {
    use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets};
    use haider_protocol::ids::{DeviceId, EventId};
    EventEnvelope {
        schema_version: 1,
        event_id: EventId::new(format!("evt-{seq}")),
        seq,
        session_id: session.clone(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("dev"),
        authority_epoch: 1,
        worker_generation: 1,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::to_value(haider_protocol::EventPayload::RunState(state))
            .expect("payload serializes"),
    }
}

#[test]
fn background_session_terminal_fires_a_desktop_notification() {
    // M10: a turn in a BACKGROUND session (one not checked out on screen) still
    // fires a desktop notification when it reaches a terminal state. `route_raw`
    // is the real runtime's single entry point for the event stream; the
    // attached reducer (`handle_envelope`) only ever evaluated the ACTIVE
    // session, so a backgrounded turn's Done/Errored used to notify never.
    use haider_tui::identity::UiGeneration;
    use haider_tui::session::SessionState;

    let mut model = AppModel::new();
    model.mode = RuntimeMode::Live;
    // A DIFFERENT session is attached, so the one under test is truly
    // background. Focus starts unreported → the focus gate does not suppress.
    model.active_session = Some(SessionId::new("attached"));
    let bg = SessionId::new("bg-session");
    let mut slot = SessionState::neutral(bg.clone(), UiGeneration::new(9));
    slot.title = Some("nightly deploy".to_owned());
    model.sessions.push(slot);

    // The background turn reaches a terminal state.
    model.route_raw(&run_state_envelope(&bg, 1, RunState::Done));
    assert!(
        model
            .notifications
            .iter()
            .any(|line| line.contains("turn done")),
        "a background terminal transition fires a notification: {:?}",
        model.notifications
    );
    // Per-session edge: the SAME terminal state (a later envelope) does not
    // re-notify — one ping per background turn.
    model.route_raw(&run_state_envelope(&bg, 2, RunState::Done));
    assert_eq!(
        model.notifications.len(),
        1,
        "one notification per background turn: {:?}",
        model.notifications
    );
}
