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
            TranscriptEntry::Error { text, .. } => Some(text.clone()),
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
            presentation: None,
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
            presentation: None,
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
        workspace_mutation: None,
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
            presentation: None,
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
                name: "fs_edit".to_owned(),
                args: serde_json::json!({"path": "x.rs"}),
                status: ToolStatus::Failed,
                call_id: String::new(),
            },
        }),
    ));
    assert!(
        draw_text(&model).contains("✗ fs_edit"),
        "the failed tool is marked in the view"
    );
}

// ── One failure, one row (dogfood bug 3) ────────────────────────────────
//
// A failed tool call must render ONCE: inline on its tool row. The
// standalone `effect failed — …` line remains ONLY for failures with no
// owning tool-call row, and a deferred failure that never finds its owner
// flushes at the run's terminal state so nothing is silently swallowed.

fn effect_intent(seq_effect: &str, summary: &str) -> EventPayload {
    EventPayload::Effect(EffectPhase::Intent(haider_protocol::effect::EffectIntent {
        effect: EffectId::new(seq_effect),
        class: haider_protocol::effect::EffectClass::ProcessExec,
        summary: summary.to_owned(),
        args_digest: format!("digest-{seq_effect}"),
        workspace_revision: None,
    }))
}

fn effect_failed(effect: &str, error: &str) -> EventPayload {
    EventPayload::Effect(EffectPhase::Outcome {
        effect: EffectId::new(effect),
        outcome: EffectOutcome::Failed {
            error: error.to_owned(),
        },
        freshness: None,
        workspace_mutation: None,
    })
}

fn failed_result(reason: &str) -> haider_protocol::tool::BoundedResult {
    haider_protocol::tool::BoundedResult {
        preview: String::new(),
        truncated: false,
        artifact: None,
        images: Vec::new(),
        cursor: None,
        status: haider_protocol::tool::ToolResultStatus::Failed,
        reason: Some(reason.to_owned()),
        presentation: None,
    }
}

fn tool_started(item: &str, call: &str) -> EventPayload {
    EventPayload::Item(ItemEvent::Started {
        item_id: ItemId::new(item),
        item: TurnItem::ToolCall {
            name: "process_exec".to_owned(),
            args: serde_json::json!({"cmd": "cargo test"}),
            status: ToolStatus::InProgress,
            call_id: call.to_owned(),
        },
    })
}

fn tool_completed(item: &str, call: &str, status: ToolStatus) -> EventPayload {
    EventPayload::Item(ItemEvent::Completed {
        item_id: ItemId::new(item),
        item: TurnItem::ToolCall {
            name: "process_exec".to_owned(),
            args: serde_json::json!({"cmd": "cargo test"}),
            status,
            call_id: call.to_owned(),
        },
    })
}

/// MUTATION CHECK (dogfood bug 3): restore the unconditional
/// `TranscriptEntry::Error` push in the `EffectOutcome::Failed` arm.
/// Expected runtime failure: the same failure renders twice — inline on
/// the tool row AND as a standalone `effect failed` line.
#[test]
fn a_failed_tool_call_renders_exactly_one_row() {
    let mut model = live_session();
    model.route_raw(&raw(1, &tool_started("tool-9", "call-9")));
    model.route_raw(&raw(2, &effect_intent("eff-9", "run cargo test")));
    model.route_raw(&raw(3, &effect_failed("eff-9", "spawn: ENOENT")));
    model.route_raw(&raw(
        4,
        &EventPayload::ToolResult {
            call_id: "call-9".to_owned(),
            result: failed_result("spawn: ENOENT"),
        },
    ));
    model.route_raw(&raw(
        5,
        &tool_completed("tool-9", "call-9", ToolStatus::Failed),
    ));
    model.route_raw(&raw(6, &EventPayload::RunState(RunState::Done)));
    let errors = error_texts(&model);
    assert!(
        errors.is_empty(),
        "the failure lives on the tool row only — no standalone line: {errors:?}"
    );
    let reason = model
        .projection
        .entries()
        .iter()
        .find_map(|entry| match entry {
            TranscriptEntry::Item(block) => block.tool_reason.clone(),
            _ => None,
        })
        .expect("the tool row carries its failure reason");
    assert!(reason.contains("ENOENT"), "{reason}");
    assert!(
        draw_text(&model).contains("✗ process_exec"),
        "one visible failed row"
    );
}

/// A failure with NO owning tool-call row keeps its standalone line —
/// suppression must never swallow an unowned failure.
#[test]
fn an_unowned_effect_failure_keeps_its_standalone_row() {
    let mut model = live_session();
    // Intent journals while no tool row is live: no owner candidates.
    model.route_raw(&raw(1, &effect_intent("eff-10", "background work")));
    model.route_raw(&raw(2, &effect_failed("eff-10", "disk full")));
    let errors = error_texts(&model);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("effect failed") && errors[0].contains("disk full"));
}

/// MUTATION CHECK (dogfood bug 3): drop `flush_pending_effect_failures`
/// from the terminal RunState arm. Expected runtime failure: a deferred
/// failure whose owner never settles vanishes — a silent failure.
#[test]
fn a_deferred_effect_failure_flushes_at_the_terminal_state() {
    let mut model = live_session();
    model.route_raw(&raw(1, &tool_started("tool-11", "call-11")));
    model.route_raw(&raw(2, &effect_intent("eff-11", "run cargo test")));
    model.route_raw(&raw(3, &effect_failed("eff-11", "boom")));
    assert!(
        error_texts(&model).is_empty(),
        "the failure is deferred while its owner may still settle"
    );
    model.route_raw(&raw(4, &EventPayload::RunState(RunState::Done)));
    let errors = error_texts(&model);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("effect failed") && errors[0].contains("boom"));
}

fn tool_reason_of(model: &AppModel, item: &str) -> Option<String> {
    model
        .projection
        .entries()
        .iter()
        .find_map(|entry| match entry {
            TranscriptEntry::Item(block) if block.item_id.as_str() == item => {
                block.tool_reason.clone()
            }
            _ => None,
        })
}

/// MUTATION CHECK (rev934 P3): let one settling row resolve EVERY candidate
/// failure again (drop the one-settle law or the error-evidence preference).
/// Expected runtime failure: the first-settling row below pushes the OTHER
/// effect's error as a premature standalone line, or adopts it as its own.
#[test]
fn interleaved_results_attribute_each_effect_to_its_own_row() {
    let mut model = live_session();
    model.route_raw(&raw(1, &tool_started("tool-a", "call-a")));
    model.route_raw(&raw(2, &tool_started("tool-b", "call-b")));
    model.route_raw(&raw(3, &effect_intent("eff-a", "first effect")));
    model.route_raw(&raw(4, &effect_failed("eff-a", "alpha failed")));
    model.route_raw(&raw(5, &effect_intent("eff-b", "second effect")));
    model.route_raw(&raw(6, &effect_failed("eff-b", "beta failed")));
    // Results interleave: the SECOND effect's row settles first, carrying
    // its own error. Error evidence must win over arrival order.
    model.route_raw(&raw(
        7,
        &EventPayload::ToolResult {
            call_id: "call-b".to_owned(),
            result: failed_result("beta failed"),
        },
    ));
    model.route_raw(&raw(
        8,
        &tool_completed("tool-b", "call-b", ToolStatus::Failed),
    ));
    model.route_raw(&raw(
        9,
        &EventPayload::ToolResult {
            call_id: "call-a".to_owned(),
            result: failed_result("alpha failed"),
        },
    ));
    model.route_raw(&raw(
        10,
        &tool_completed("tool-a", "call-a", ToolStatus::Failed),
    ));
    model.route_raw(&raw(11, &EventPayload::RunState(RunState::Done)));
    let errors = error_texts(&model);
    assert!(
        errors.is_empty(),
        "each failure lives inline on its own row: {errors:?}"
    );
    let reason_a = tool_reason_of(&model, "tool-a").expect("row a reason");
    let reason_b = tool_reason_of(&model, "tool-b").expect("row b reason");
    assert!(reason_a.contains("alpha failed"), "{reason_a}");
    assert!(reason_b.contains("beta failed"), "{reason_b}");
}

/// MUTATION CHECK (rev934 P3): let one bare-failure row consume BOTH pending
/// effect failures (stamp one, standalone the other) again. Expected runtime
/// failure: a premature standalone line appears at the first completion and
/// the second row completes with no reason at all.
#[test]
fn two_bare_failures_settle_one_effect_each_never_two_on_one_row() {
    let mut model = live_session();
    model.route_raw(&raw(1, &tool_started("tool-a", "call-a")));
    model.route_raw(&raw(2, &tool_started("tool-b", "call-b")));
    model.route_raw(&raw(3, &effect_intent("eff-a", "first effect")));
    model.route_raw(&raw(4, &effect_failed("eff-a", "alpha failed")));
    model.route_raw(&raw(5, &effect_intent("eff-b", "second effect")));
    model.route_raw(&raw(6, &effect_failed("eff-b", "beta failed")));
    // No ToolResult events at all: both rows complete bare, in reverse
    // order. The first-settling candidate law adopts ONE failure per row.
    model.route_raw(&raw(
        7,
        &tool_completed("tool-b", "call-b", ToolStatus::Failed),
    ));
    model.route_raw(&raw(
        8,
        &tool_completed("tool-a", "call-a", ToolStatus::Failed),
    ));
    model.route_raw(&raw(9, &EventPayload::RunState(RunState::Done)));
    let errors = error_texts(&model);
    assert!(
        errors.is_empty(),
        "one adoption per row, no standalone: {errors:?}"
    );
    let reason_b = tool_reason_of(&model, "tool-b").expect("first-settling row adopts the oldest");
    let reason_a = tool_reason_of(&model, "tool-a").expect("second row adopts the remaining");
    assert!(reason_b.contains("alpha failed"), "{reason_b}");
    assert!(reason_a.contains("beta failed"), "{reason_a}");
}

/// A result that carries a DIFFERENT error does not suppress the effect's
/// own line — two distinct failures stay two visible facts.
#[test]
fn a_mismatched_result_error_does_not_suppress_the_effect_row() {
    let mut model = live_session();
    model.route_raw(&raw(1, &tool_started("tool-12", "call-12")));
    model.route_raw(&raw(2, &effect_intent("eff-12", "run cargo test")));
    model.route_raw(&raw(3, &effect_failed("eff-12", "spawn: ENOENT")));
    model.route_raw(&raw(
        4,
        &EventPayload::ToolResult {
            call_id: "call-12".to_owned(),
            result: failed_result("different problem"),
        },
    ));
    let errors = error_texts(&model);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("effect failed") && errors[0].contains("ENOENT"));
}
