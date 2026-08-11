//! Durable per-session prompt backtracking (Claude Code-style Esc Esc).
#![allow(clippy::expect_used)]

use std::time::{Duration, Instant};

use haider_protocol::EventPayload;
use haider_protocol::envelope::{EventEnvelope, PromptRender, RawEnvelope, RenderTargets};
use haider_protocol::ids::{DeviceId, EventId, SessionId};
use haider_tui::app::{AppEvent, AppModel, AppRequest, RuntimeMode};
use haider_tui::projection::RawOutcome;
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

mod common;
use common::{launcher_model, run_slash};

fn key(code: KeyCode) -> AppEvent {
    AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn raw(session: &SessionId, seq: u64, text: &str) -> RawEnvelope {
    EventEnvelope {
        schema_version: 1,
        event_id: EventId::new(format!("history-{seq}")),
        seq,
        session_id: session.clone(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("history-device"),
        authority_epoch: 1,
        worker_generation: 1,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: seq,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::to_value(EventPayload::UserMessage {
            text: text.to_owned(),
            attachments: vec![],
            mode: haider_protocol::DeliveryMode::Steer,
        })
        .expect("user message serializes"),
    }
}

fn replayed_model() -> (AppModel, SessionId) {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    let session = model.sessions[0].id.clone();
    assert_eq!(
        model.route_raw(&raw(&session, 1, "oldest\nverbatim")),
        RawOutcome::Applied
    );
    assert_eq!(
        model.route_raw(&raw(&session, 2, "newest prompt")),
        RawOutcome::Applied
    );
    model.open_session(&session);
    model.turn_active = false;
    model.requests.clear();
    (model, session)
}

fn open_backtrack(model: &mut AppModel, now: Instant) {
    model.handle_at(key(KeyCode::Esc), now);
    assert!(
        model.backtrack.is_none(),
        "the first Esc only arms the gesture"
    );
    model.handle_at(key(KeyCode::Esc), now + Duration::from_millis(100));
    assert!(
        model.backtrack.is_some(),
        "the rapid second Esc opens history"
    );
}

fn draw(model: &AppModel) -> String {
    let backend = TestBackend::new(96, 34);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            let _ = render(model, frame);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer();
    let mut text = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            text.push_str(buffer[(x, y)].symbol());
        }
        text.push('\n');
    }
    text
}

/// LAW C1: history is rebuilt solely from admitted journal user messages,
/// stored newest-first, and travels with the session across checkout/restart.
#[test]
fn journal_replay_populates_per_session_history_newest_first() {
    let (mut model, session) = replayed_model();
    assert_eq!(
        model.prompt_history.iter().cloned().collect::<Vec<_>>(),
        ["newest prompt", "oldest\nverbatim"]
    );
    model.checkin();
    assert!(model.prompt_history.is_empty());
    model.open_session(&session);
    assert_eq!(
        model.prompt_history.iter().cloned().collect::<Vec<_>>(),
        ["newest prompt", "oldest\nverbatim"]
    );
}

/// LAW C2/C5: only idle + truly empty composer admits the double-Esc
/// gesture. During streaming the first Esc remains an interrupt and cannot
/// open or advance prompt history.
#[test]
fn esc_interrupt_precedes_backtrack_and_backtrack_requires_idle_empty() {
    let (mut model, _) = replayed_model();
    let now = Instant::now();
    model.turn_active = true;
    model.handle_at(key(KeyCode::Esc), now);
    assert!(model.backtrack.is_none());
    assert!(!model.turn_active);
    assert!(
        model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::Interrupt { .. }))
    );
    model.handle_at(key(KeyCode::Esc), now + Duration::from_millis(50));
    assert!(
        model.backtrack.is_none(),
        "post-interrupt Esc is the first idle Esc"
    );

    model.handle(key(KeyCode::Char('x')));
    model.handle_at(key(KeyCode::Esc), now + Duration::from_secs(1));
    model.handle_at(key(KeyCode::Esc), now + Duration::from_millis(1_050));
    assert!(
        model.backtrack.is_none(),
        "non-empty drafts keep ordinary Esc semantics"
    );
}

/// LAW C3/C4: repeated rapid Esc walks older entries; Enter loads the exact
/// journal bytes into the composer. A second Enter emits a new submit request
/// without editing prior history; only the new journal echo appends it.
#[test]
fn repeated_escape_loads_verbatim_and_redo_appends_only_on_journal_echo() {
    let (mut model, session) = replayed_model();
    let now = Instant::now();
    open_backtrack(&mut model, now);
    model.handle_at(key(KeyCode::Esc), now + Duration::from_millis(200));
    assert_eq!(model.backtrack.expect("chooser").selection, 1);
    model.handle_at(key(KeyCode::Enter), now + Duration::from_millis(250));
    assert_eq!(model.composer.text(), "oldest\nverbatim");
    assert!(model.backtrack.is_none());

    let prior = model.prompt_history.clone();
    model.handle(key(KeyCode::Enter));
    assert_eq!(
        model.prompt_history, prior,
        "submission does not rewrite journal history"
    );
    assert!(model.requests.iter().any(|request| matches!(
        request,
        AppRequest::SubmitText { text, .. } if text == "oldest\nverbatim"
    )));

    assert_eq!(
        model.route_raw(&raw(&session, 3, "oldest\nverbatim")),
        RawOutcome::Applied
    );
    assert_eq!(model.prompt_history.len(), 3);
    assert_eq!(model.prompt_history[0], "oldest\nverbatim");
    assert_eq!(model.prompt_history[1], "newest prompt");
    assert_eq!(model.prompt_history[2], "oldest\nverbatim");
}

/// Plain/frontends parity: `/history` reaches the same durable list when a
/// terminal cannot convey rapid double-Esc timing; an ordinal loads verbatim.
#[test]
fn history_command_loads_a_durable_prompt_by_newest_first_ordinal() {
    let (mut model, _) = replayed_model();
    run_slash(&mut model, "/history 2");
    assert_eq!(model.composer.text(), "oldest\nverbatim");
    assert!(model.backtrack.is_none());
}

/// The chooser occupies the palette band above the composer and flattens
/// multiline prompts for display without changing the journal bytes.
#[test]
fn backtrack_overlay_renders_compact_single_line_prompt_rows() {
    let (mut model, _) = replayed_model();
    open_backtrack(&mut model, Instant::now());
    let screen = draw(&model);
    assert!(screen.contains("1. newest prompt"));
    assert!(screen.contains("2. oldest verbatim"));
    assert!(screen.contains("digits choose · ⏎ load · esc older / close"));
    assert_eq!(model.prompt_history[1], "oldest\nverbatim");
}
