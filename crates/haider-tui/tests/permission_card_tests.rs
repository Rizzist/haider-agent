//! TUI computer OS-permission grant-card laws: the additive
//! `permission_grant_needed` event (outside `EventPayload`) becomes a session
//! card with clickable Open Settings / Retry actions, its keys wire to the
//! client, and `permission_grant_resolved` clears it. The card enriches the
//! paired blocking `computer-os-permission` menu; Retry reuses that menu's
//! answer path so there is one authorization channel.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::envelope::{EventEnvelope, PromptRender, RawEnvelope, RenderTargets};
use haider_protocol::ids::{DeviceId, EffectId, EventId, MenuId, SessionId};
use haider_protocol::menu::{DecisionKind, Menu, MenuKind, MenuOption, MenuScope};
use haider_protocol::permission::{
    PermissionEventPayload, PermissionGrantAction, PermissionGrantNeeded,
    PermissionGrantResolution, PermissionGrantResolved, SystemPermission,
};
use haider_tui::app::{AppModel, AppRequest, Hit, Screen};
use haider_tui::projection::SessionProjection;
use haider_tui::render::render;
use haider_tui::session::route_permission_event;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::Rect;

mod common;
use common::key;

const MENU_ID: &str = "computer-os-permission-effect-1-screen_recording";
const REQUEST_ID: &str = MENU_ID;

fn card(permission: SystemPermission, auto_restart_pending: bool) -> PermissionGrantNeeded {
    PermissionGrantNeeded {
        request_id: REQUEST_ID.to_owned(),
        menu_id: MenuId::new(MENU_ID),
        request_seq: 5,
        opening_generation: 9,
        call_id: "call-1".to_owned(),
        effect_id: EffectId::new("effect-1"),
        permission,
        pane_name: "System Settings > Privacy & Security".to_owned(),
        settings_url: "x-apple.systempreferences:com.apple.preference.security".to_owned(),
        actions: vec![
            PermissionGrantAction::OpenSettings,
            PermissionGrantAction::Retry,
            PermissionGrantAction::RestartDaemon,
        ],
        auto_restart_pending,
        poll_timeout_ms: 120_000,
    }
}

fn permission_menu() -> Menu {
    Menu {
        id: MenuId::new(MENU_ID),
        kind: MenuKind::Permission {
            effect_summary: "computer requires Screen Recording".to_owned(),
        },
        title: "Allow Screen Recording".to_owned(),
        body: vec!["macOS requires a real user grant.".to_owned()],
        options: vec![MenuOption {
            key: "retry".to_owned(),
            label: "Retry".to_owned(),
            detail: None,
            decision: Some(DecisionKind::AllowOnce),
        }],
        blocking: true,
        scope: MenuScope::Session,
        origin: "computer-os-permission".to_owned(),
        ttl_ms: None,
        timeout_option: None,
    }
}

fn raw(session: &SessionId, seq: u64, payload: serde_json::Value) -> RawEnvelope {
    EventEnvelope {
        schema_version: 1,
        event_id: EventId::new(format!("evt-{seq}")),
        seq,
        session_id: session.clone(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("perm-device"),
        authority_epoch: 1,
        worker_generation: 9,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: payload.into(),
    }
}

fn draw(model: &AppModel) -> (Vec<String>, Vec<(Rect, Hit)>) {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut hits = Vec::new();
    terminal
        .draw(|frame| hits = render(model, frame))
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let mut rows = Vec::new();
    for y in 0..buffer.area.height {
        let mut line = String::new();
        for x in 0..buffer.area.width {
            line.push_str(buffer[(x, y)].symbol());
        }
        rows.push(line);
    }
    (rows, hits)
}

/// A session-screen model with an attached session, ready for card injection.
fn card_model() -> AppModel {
    let mut model = AppModel::new();
    let session = SessionId::new("perm-session");
    model.screen = Screen::Session;
    model.active_session = Some(session);
    model
}

/// MUTATION CHECK: swallow the additive event, or clear the card on the wrong
/// request id. Expected RUNTIME failure: the card is missing after the needed
/// event, still present after its resolution, or a non-permission payload is
/// falsely consumed.
#[test]
fn permission_events_set_and_clear_the_card() {
    let mut projection = SessionProjection::new();
    let session = SessionId::new("perm-session");

    let needed = PermissionEventPayload::PermissionGrantNeeded(card(
        SystemPermission::ScreenRecording,
        false,
    ));
    let env = raw(&session, 1, needed.to_payload_value().expect("serialize"));
    assert!(route_permission_event(&mut projection, &env));
    assert_eq!(
        projection.permission_card().map(|c| c.request_id.as_str()),
        Some(REQUEST_ID)
    );

    // A non-permission payload is not consumed by this router.
    let other = raw(
        &session,
        2,
        serde_json::to_value(EventPayload::IdleDecayed).expect("serialize"),
    );
    assert!(!route_permission_event(&mut projection, &other));

    // A resolution for a DIFFERENT request must not drop the live card.
    let stale = PermissionEventPayload::PermissionGrantResolved(PermissionGrantResolved {
        request_id: "some-other-request".to_owned(),
        permission: SystemPermission::ScreenRecording,
        resolution: PermissionGrantResolution::Granted,
        retrying_parked_action: true,
    });
    let stale_env = raw(&session, 3, stale.to_payload_value().expect("serialize"));
    assert!(route_permission_event(&mut projection, &stale_env));
    assert!(projection.permission_card().is_some());

    // The matching resolution clears it.
    let resolved = PermissionEventPayload::PermissionGrantResolved(PermissionGrantResolved {
        request_id: REQUEST_ID.to_owned(),
        permission: SystemPermission::ScreenRecording,
        resolution: PermissionGrantResolution::Granted,
        retrying_parked_action: true,
    });
    let resolved_env = raw(&session, 4, resolved.to_payload_value().expect("serialize"));
    assert!(route_permission_event(&mut projection, &resolved_env));
    assert!(projection.permission_card().is_none());
}

/// MUTATION CHECK: drop the buttons, or stop labelling the permission.
/// Expected RUNTIME failure: the card text or its two action hits are missing.
#[test]
fn card_renders_labelled_prompt_with_open_settings_and_retry() {
    let mut model = card_model();
    model
        .projection
        .set_permission_card(card(SystemPermission::ScreenRecording, false));
    let (rows, hits) = draw(&model);
    let text = rows.join("\n");
    assert!(text.contains("Screen Recording"), "card text:\n{text}");
    assert!(text.contains("Open Settings"), "card text:\n{text}");
    assert!(text.contains("Retry"), "card text:\n{text}");
    assert!(
        hits.iter().any(|(_, h)| *h == Hit::PermissionOpenSettings),
        "missing Open Settings hit"
    );
    assert!(
        hits.iter().any(|(_, h)| *h == Hit::PermissionRetry),
        "missing Retry hit"
    );
}

/// MUTATION CHECK: keep offering Retry once a restart is pending (a recheck
/// cannot help), or drop the granted state. Expected RUNTIME failure: the
/// restart card still exposes a Retry hit, or omits the granted/restart text.
#[test]
fn granted_card_shows_restart_and_drops_retry() {
    let mut model = card_model();
    model
        .projection
        .set_permission_card(card(SystemPermission::ScreenRecording, true));
    let (rows, hits) = draw(&model);
    let text = rows.join("\n");
    assert!(text.contains("granted"), "card text:\n{text}");
    assert!(text.contains("Restart Haider"), "card text:\n{text}");
    assert!(
        hits.iter().any(|(_, h)| *h == Hit::PermissionOpenSettings),
        "Open Settings stays available after grant"
    );
    assert!(
        !hits.iter().any(|(_, h)| *h == Hit::PermissionRetry),
        "a restart-pending card must not offer Retry"
    );
}

/// MUTATION CHECK: stop routing the `o` key, or forget the session. Expected
/// RUNTIME failure: pressing `o` enqueues no open-settings request for the
/// parked permission.
#[test]
fn key_o_requests_open_settings_for_the_parked_permission() {
    let mut model = card_model();
    model
        .projection
        .set_permission_card(card(SystemPermission::Accessibility, false));
    model.handle(key(KeyCode::Char('o')));
    let request = model.requests.iter().find_map(|request| match request {
        AppRequest::OpenPermissionSettings {
            request_id,
            permission,
            ..
        } => Some((request_id.clone(), *permission)),
        _ => None,
    });
    assert_eq!(
        request,
        Some((REQUEST_ID.to_owned(), SystemPermission::Accessibility))
    );
}

/// MUTATION CHECK: answer a different menu, or open a second authorization
/// channel. Expected RUNTIME failure: pressing `r` does not answer the paired
/// `computer-os-permission` menu's retry option.
#[test]
fn key_r_answers_the_paired_permission_menu() {
    let mut model = card_model();
    model
        .projection
        .apply(&EventPayload::MenuOpened(permission_menu()));
    model
        .projection
        .set_permission_card(card(SystemPermission::ScreenRecording, false));
    model.handle(key(KeyCode::Char('r')));
    let answer = model
        .outbox
        .iter()
        .find(|outbound| outbound.answer.menu.as_str() == MENU_ID);
    assert!(answer.is_some(), "retry must answer the permission menu");
    assert_eq!(
        answer.and_then(|outbound| outbound.answer.option_key.as_deref()),
        Some("retry")
    );
}

/// v0.0.938: the grant card is TURN-SCOPED. It exists to enrich a blocking
/// menu that parks the CURRENT turn ("grant it, then Retry — it resumes
/// automatically"), so when that turn reaches a terminal state the card must
/// go with it. Its only other exit is a matching `permission_grant_resolved`,
/// which a CANCELLED turn never produces — so before this the card outlived
/// its turn and sat over an idle session offering a Retry with nothing left
/// to resume (owner-reported: cancelled the turn, card stayed).
///
/// MUTATION CHECK (executed): drop the terminal-state clear in
/// `SessionProjection::apply`'s `RunState` arm and the cancelled assertion
/// fails — the card survives into idle exactly as reported.
#[test]
fn a_cancelled_turn_takes_the_permission_card_with_it() {
    use haider_protocol::state::RunState;

    for terminal in [RunState::Cancelled, RunState::Done] {
        let mut projection = SessionProjection::new();
        let session = SessionId::new("perm-cancel");

        // The turn parks on the OS-permission menu and the card enriches it.
        let needed = PermissionEventPayload::PermissionGrantNeeded(card(
            SystemPermission::ScreenRecording,
            false,
        ));
        let env = raw(&session, 1, needed.to_payload_value().expect("serialize"));
        assert!(route_permission_event(&mut projection, &env));
        assert!(
            projection.permission_card().is_some(),
            "the card is present while the turn is parked"
        );

        // The turn ends without the grant ever being resolved.
        projection.apply(&EventPayload::RunState(terminal.clone()));
        assert!(
            projection.permission_card().is_none(),
            "{terminal:?} must take the card with it — no Retry over an idle session"
        );
    }
}

/// The card SURVIVES a non-terminal state change: parking, streaming and
/// tool-running are all still the same turn, and clearing there would erase
/// the card the moment anything else happened mid-park.
#[test]
fn a_live_turn_keeps_its_permission_card() {
    use haider_protocol::state::RunState;

    let mut projection = SessionProjection::new();
    let session = SessionId::new("perm-live");
    let needed = PermissionEventPayload::PermissionGrantNeeded(card(
        SystemPermission::ScreenRecording,
        false,
    ));
    let env = raw(&session, 1, needed.to_payload_value().expect("serialize"));
    assert!(route_permission_event(&mut projection, &env));

    projection.apply(&EventPayload::RunState(RunState::Streaming));
    assert!(
        projection.permission_card().is_some(),
        "a live turn keeps its card"
    );
}
