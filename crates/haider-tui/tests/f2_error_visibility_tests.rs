//! F2e — the error-visibility sweep: EVERY turn-level failure the wire
//! can carry surfaces as a visible session-view line with its public
//! reason. A turn must never end in a silent IDLE.
//!
//! One law per wire kind:
//! * `RunFailed`             → `{code} — {message}` error line (W5g-6 pin)
//! * `RunState::Errored`     → synthesized line when NO reason was paired
//! * `EffectOutcome::Failed` → `effect failed — {error}`
//! * `EffectOutcome::CancelledEscalated` → `effect cancel escalated — …`
//! * `EffectOutcome::Unknown`→ crash-window line
//! * red `GateReport` verdicts (errored/failed-env/incomplete/ack-red)
//! * `Rotation`              → visible note, like a model change (§4.4)
//! * a rejected `turn.submit` → session-view line, not just a flash
//! * `ToolStatus::Failed`    → the ✗ glyph on the tool row (pin)
//! * `session.select_model` refusals — pinned in f2_model_picker_tests.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::credential::{RotationCause, RotationEvent};
use haider_protocol::effect::{EffectOutcome, EffectPhase};
use haider_protocol::envelope::{EventEnvelope, PromptRender, RawEnvelope, RenderTargets};
use haider_protocol::ids::{CredentialAlias, DeviceId, EffectId, EventId, ItemId, SessionId};
use haider_protocol::item::{ItemEvent, ToolStatus, TurnItem};
use haider_protocol::state::RunState;
use haider_protocol::verify::{GateReport, VerifyVerdict};
use haider_tui::app::{AppModel, RuntimeMode};
use haider_tui::live::{LiveCommand, LiveDriver, LiveReply};
use haider_tui::projection::TranscriptEntry;
use haider_tui::render::render;
use haider_tui::runtime::live_pass;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::KeyCode;

mod common;
use common::{key, launcher_model};

fn sid() -> SessionId {
    SessionId::new("f2e-session")
}

fn raw(seq: u64, payload: &EventPayload) -> RawEnvelope {
    EventEnvelope {
        schema_version: 1,
        event_id: EventId::new(format!("evt-f2e-{seq}")),
        seq,
        session_id: sid(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("f2e-device"),
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
        payload: serde_json::to_value(payload).expect("payload serializes"),
    }
}

fn live_session() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.sessions.clear();
    model.upsert_live_session(&sid());
    model.open_session(&sid());
    model.requests.clear();
    model
}

fn error_texts(model: &AppModel) -> Vec<String> {
    model
        .projection
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::Error { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn note_texts(model: &AppModel) -> Vec<String> {
    model
        .projection
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::Note { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn draw_text(model: &AppModel) -> String {
    let backend = TestBackend::new(110, 32);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            render(model, frame);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// PIN (W5g-6): `RunFailed` puts `{code} — {message}` in the view.
#[test]
fn run_failed_reason_is_a_visible_error_line() {
    let mut model = live_session();
    model.route_raw(&raw(
        1,
        &EventPayload::RunFailed {
            code: haider_protocol::error::ErrorCode::ProviderError,
            message: "upstream 529 — overloaded".to_owned(),
            retryable: true,
        },
    ));
    let errors = error_texts(&model);
    assert!(
        errors
            .iter()
            .any(|text| text.contains("provider_error") && text.contains("overloaded")),
        "the public code and message reach the view: {errors:?}"
    );
    assert!(
        draw_text(&model).contains("✗"),
        "and render with the ✗ mark"
    );
}

/// MUTATION CHECK (F2e): drop the unpaired-Errored synthesizer in the
/// RunState arm. Expected runtime failure: the badge goes ✗ ERRORED with
/// an empty transcript — the exact silent-IDLE class this sweep kills.
#[test]
fn errored_without_a_paired_reason_synthesizes_a_line() {
    let mut model = live_session();
    model.route_raw(&raw(1, &EventPayload::RunState(RunState::Thinking)));
    model.route_raw(&raw(2, &EventPayload::RunState(RunState::Errored)));
    let errors = error_texts(&model);
    assert_eq!(errors.len(), 1, "exactly one synthesized line");
    assert!(
        errors[0].contains("no public reason"),
        "the line says what is known — nothing: {errors:?}"
    );
}

/// The synthesizer NEVER doubles a real reason: RunFailed then Errored
/// yields exactly the one real line.
#[test]
fn a_paired_reason_is_never_doubled() {
    let mut model = live_session();
    model.route_raw(&raw(1, &EventPayload::RunState(RunState::Thinking)));
    model.route_raw(&raw(
        2,
        &EventPayload::RunFailed {
            code: haider_protocol::error::ErrorCode::Unauthorized,
            message: "oauth refresh failed".to_owned(),
            retryable: false,
        },
    ));
    model.route_raw(&raw(3, &EventPayload::RunState(RunState::Errored)));
    let errors = error_texts(&model);
    assert_eq!(errors.len(), 1, "one failure, one line: {errors:?}");
    assert!(errors[0].contains("oauth refresh failed"));
    // And the NEXT turn re-arms the synthesizer.
    model.route_raw(&raw(4, &EventPayload::RunState(RunState::Thinking)));
    model.route_raw(&raw(5, &EventPayload::RunState(RunState::Errored)));
    assert_eq!(error_texts(&model).len(), 2, "the next turn is covered too");
}

fn outcome_event(outcome: EffectOutcome) -> EventPayload {
    EventPayload::Effect(EffectPhase::Outcome {
        effect: EffectId::new("eff-1"),
        outcome,
        freshness: None,
    })
}

/// `EffectOutcome::Failed` → a visible line with the error.
#[test]
fn effect_failed_is_visible() {
    let mut model = live_session();
    model.route_raw(&raw(
        1,
        &outcome_event(EffectOutcome::Failed {
            error: "spawn: ENOENT".to_owned(),
        }),
    ));
    let errors = error_texts(&model);
    assert!(
        errors
            .iter()
            .any(|text| text.contains("effect failed") && text.contains("ENOENT")),
        "{errors:?}"
    );
}

/// `EffectOutcome::CancelledEscalated` → its note is visible.
#[test]
fn effect_cancel_escalation_is_visible() {
    let mut model = live_session();
    model.route_raw(&raw(
        1,
        &outcome_event(EffectOutcome::CancelledEscalated {
            note: "process group death unproven".to_owned(),
        }),
    ));
    let errors = error_texts(&model);
    assert!(
        errors
            .iter()
            .any(|text| text.contains("cancel escalated") && text.contains("unproven")),
        "{errors:?}"
    );
}

/// `EffectOutcome::Unknown` → the crash window is named in the view.
#[test]
fn effect_unknown_outcome_is_visible() {
    let mut model = live_session();
    model.route_raw(&raw(1, &outcome_event(EffectOutcome::Unknown)));
    let errors = error_texts(&model);
    assert!(
        errors
            .iter()
            .any(|text| text.contains("effect outcome unknown")),
        "{errors:?}"
    );
    // The benign outcomes stay quiet.
    model.route_raw(&raw(2, &outcome_event(EffectOutcome::Ok)));
    model.route_raw(&raw(3, &outcome_event(EffectOutcome::Cancelled)));
    assert_eq!(error_texts(&model).len(), 1, "ok/cancelled add nothing");
}

fn gate(verdict: VerifyVerdict) -> EventPayload {
    EventPayload::GateReport(GateReport {
        verdict,
        new_errors: Vec::new(),
        new_warnings: Vec::new(),
        preexisting: 0,
        cycles_used: 1,
        duration_ms: 10,
        raw_log: None,
        format: None,
        tests: None,
    })
}

/// Every red gate verdict lands a line naming its kind; green/waived
/// verdicts stay quiet.
#[test]
fn red_gate_verdicts_are_visible_and_green_stays_quiet() {
    let cases: [(VerifyVerdict, &str); 4] = [
        (VerifyVerdict::ErroredWithReport, "cycle cap exhausted"),
        (
            VerifyVerdict::FailedEnv {
                item: "rustup missing".to_owned(),
            },
            "rustup missing",
        ),
        (
            VerifyVerdict::Incomplete {
                reason: "verifier timeout".to_owned(),
            },
            "verifier timeout",
        ),
        (VerifyVerdict::AcknowledgedRed, "acknowledged-red"),
    ];
    for (verdict, marker) in cases {
        let mut model = live_session();
        model.route_raw(&raw(1, &gate(verdict.clone())));
        let errors = error_texts(&model);
        assert!(
            errors
                .iter()
                .any(|text| text.starts_with("verify") && text.contains(marker)),
            "verdict {verdict:?} must surface with {marker:?}: {errors:?}"
        );
    }
    let mut model = live_session();
    model.route_raw(&raw(1, &gate(VerifyVerdict::NotApplicable)));
    model.route_raw(&raw(
        2,
        &gate(VerifyVerdict::Waived {
            reason: "docs-only".to_owned(),
        }),
    ));
    assert!(error_texts(&model).is_empty(), "quiet verdicts stay quiet");
}

/// A credential rotation surfaces like a model change (§4.4): a visible
/// note naming the new account and the public cause.
#[test]
fn rotation_is_visible_like_a_model_change() {
    let mut model = live_session();
    model.route_raw(&raw(
        1,
        &EventPayload::Rotation(RotationEvent {
            provider: "anthropic".to_owned(),
            from: CredentialAlias::new("personal-max"),
            to: CredentialAlias::new("backup-key"),
            cause: RotationCause::RateLimit,
        }),
    ));
    let notes = note_texts(&model);
    assert!(
        notes.iter().any(|text| text.contains("account rotated")
            && text.contains("backup-key")
            && text.contains("rate limit")),
        "the rotation names target and cause: {notes:?}"
    );
}

/// MUTATION CHECK (F2e): drop the `record_session_error` call from the
/// rejected-submit arm. Expected runtime failure: the rejection stays a
/// transient flash, the transcript has no line, and this law fails on
/// the empty error list.
#[test]
fn a_rejected_submit_lands_in_the_session_view() {
    let mut model = live_session();
    let mut driver = LiveDriver::new("test");
    for c in "hello".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    let pass = live_pass(&mut driver, &mut model, None, std::time::Instant::now());
    let command_id = pass
        .commands
        .iter()
        .find_map(|command| match command {
            LiveCommand::Submit { command_id, .. } => Some(command_id.clone()),
            _ => None,
        })
        .expect("the submit issued");
    assert!(model.turn_active, "the optimistic turn is live");
    live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Failed {
            command_id: Some(command_id),
            code: "unauthorized".to_owned(),
            message: "oauth token expired".to_owned(),
            retryable: false,
        }),
        std::time::Instant::now(),
    );
    assert!(!model.turn_active, "the dead turn releases the UI");
    let errors = error_texts(&model);
    assert!(
        errors.iter().any(|text| text.contains("submit rejected")
            && text.contains("unauthorized")
            && text.contains("oauth token expired")),
        "the public reason reaches the session view: {errors:?}"
    );
    assert!(
        draw_text(&model).contains("submit rejected"),
        "and it renders"
    );
}

/// PIN: a failed tool row wears the ✗ glyph in the session view.
#[test]
fn failed_tool_rows_wear_the_error_glyph() {
    let mut model = live_session();
    model.route_raw(&raw(
        1,
        &EventPayload::Item(ItemEvent::Started {
            item_id: ItemId::new("tool-1"),
            item: TurnItem::ToolCall {
                name: "fs_patch".to_owned(),
                args: serde_json::json!({"path": "x.rs"}),
                status: ToolStatus::Failed,
                call_id: String::new(),
            },
        }),
    ));
    assert!(
        draw_text(&model).contains("✗ fs_patch"),
        "the failed tool is marked in the view"
    );
}
