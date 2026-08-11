//! W-A background task rows (LT3 projection half + decision 7): the
//! additive task-event union paints ambient transcript notes (started +
//! completion with exit and tail), the running band ticks its elapsed on
//! the journal clock above the composer and sheds when the task ends, and
//! plain mode prints the same lines through the shared note voice.
#![allow(clippy::expect_used)]

use haider_protocol::envelope::{EventEnvelope, PromptRender, RawEnvelope, RenderTargets};
use haider_protocol::ids::{DeviceId, EventId, SessionId, TaskId};
use haider_protocol::task::{
    TaskCompleted, TaskCompletionDelivery, TaskEventPayload, TaskStarted, TaskTerminalState,
};
use haider_tui::app::{AppModel, Hit, RuntimeMode, Screen};
use haider_tui::projection::TranscriptEntry;
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;

mod common;
use common::launcher_model;

fn sid() -> SessionId {
    SessionId::new("s-wa-tasks")
}

fn raw_task(seq: u64, at_ms: u64, payload: &TaskEventPayload) -> RawEnvelope {
    EventEnvelope {
        schema_version: 1,
        event_id: EventId::new(format!("evt-wa-{seq}")),
        seq,
        session_id: sid(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("wa-device"),
        authority_epoch: 1,
        worker_generation: 7,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: at_ms,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Verbatim,
        },
        payload: payload.to_payload_value().expect("task payload"),
    }
}

fn started(task: &str, name: &str, command: &str, at_ms: u64) -> TaskEventPayload {
    TaskEventPayload::TaskStarted(TaskStarted {
        task: TaskId::new(task),
        name: name.to_owned(),
        command: command.to_owned(),
        pid: 4242,
        started_at_ms: at_ms,
    })
}

fn completed(task: &str, name: &str, state: TaskTerminalState, tail: &str) -> TaskEventPayload {
    TaskEventPayload::TaskCompleted(TaskCompleted {
        task: TaskId::new(task),
        name: name.to_owned(),
        state,
        elapsed_ms: 42_000,
        output_bytes: 512,
        tail: tail.to_owned(),
        artifact: None,
        truncated: false,
        full_output_unavailable: false,
        delivery: TaskCompletionDelivery::DeliveredQueued,
    })
}

#[test]
fn e1a_task_cas_loss_keeps_tail_and_marks_full_output_unavailable() {
    let mut fact = completed(
        "task-cas-loss",
        "builder",
        TaskTerminalState::Failed {
            reason: "process exited with code 1".into(),
        },
        "last useful output",
    );
    let TaskEventPayload::TaskCompleted(completed) = &mut fact else {
        unreachable!();
    };
    completed.full_output_unavailable = true;
    let note = haider_tui::taskrows::task_note(&fact);
    assert!(note.contains("full output unavailable"));
    assert!(note.contains("last useful output"));
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

fn draw(model: &AppModel, width: u16, height: u16) -> (Vec<String>, Vec<(Rect, Hit)>) {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut hits = Vec::new();
    terminal
        .draw(|frame| hits = render(model, frame))
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let rows = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect();
    (rows, hits)
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

/// MUTATION CHECK: drop the `route_task_event` fallback in the raw
/// decode-error arm, or change the note literals. Expected RUNTIME failure:
/// the facts count unknown instead of painting, or the started/completion
/// note text (exit + elapsed + tail) changes.
#[test]
fn task_facts_paint_started_and_completion_notes_and_never_count_unknown() {
    let mut model = live_session();
    model.route_raw(&raw_task(
        1,
        10_000,
        &started("task-a", "build", "cargo build --release", 10_000),
    ));
    model.route_raw(&raw_task(
        2,
        52_000,
        &completed(
            "task-a",
            "build",
            TaskTerminalState::Completed { exit_code: Some(0) },
            "Finished release [optimized]\n",
        ),
    ));
    let notes = note_texts(&model);
    assert_eq!(
        notes,
        [
            "⚙ task started — build · cargo build --release",
            "└ task build — exit 0 · 42s — Finished release [optimized]",
        ]
    );
    assert_eq!(
        model.projection.unknown_payloads(),
        0,
        "task facts are consumed, never counted unknown"
    );

    // Failure/kill dispositions wear their own honest words.
    model.route_raw(&raw_task(
        3,
        60_000,
        &completed("task-b", "flaky", TaskTerminalState::Killed, ""),
    ));
    let notes = note_texts(&model);
    assert_eq!(
        notes.last().expect("kill note"),
        "└ task flaky — killed · 42s"
    );
    assert_eq!(model.projection.unknown_payloads(), 0);
}

/// MUTATION CHECK (decision 7): stop rendering the band, freeze its clock,
/// or keep terminal tasks in it. Expected RUNTIME failure: the running band
/// line (name + ticking elapsed) is missing above the composer, its elapsed
/// stops following the journal clock, the anim gate closes while a task
/// runs, or the band survives completion.
#[test]
fn running_band_ticks_on_the_journal_clock_and_sheds_at_completion() {
    let mut model = live_session();
    model.screen = Screen::Session;
    model.route_raw(&raw_task(
        1,
        10_000,
        &started("task-a", "server", "python3 -m http.server", 10_000),
    ));
    let (rows, _) = draw(&model, 100, 30);
    let band = rows
        .iter()
        .find(|row| row.contains("background task"))
        .expect("running band renders");
    assert!(
        band.contains("⚙ 1 background task — server 0s"),
        "band names the task with its elapsed, got {band:?}"
    );
    assert!(
        model.animated(),
        "a running task holds the anim gate open so the elapsed ticks"
    );

    // The journal clock advances (S4 law: committed_at_ms drives it) and
    // the SAME band line re-renders with the new elapsed.
    model.route_raw(&raw_task(
        2,
        52_000,
        &started("task-b", "watcher", "cargo watch", 52_000),
    ));
    let (rows, _) = draw(&model, 100, 30);
    let band = rows
        .iter()
        .find(|row| row.contains("background task"))
        .expect("running band renders");
    assert!(
        band.contains("⚙ 2 background tasks — server 42s · watcher 0s"),
        "elapsed follows the journal clock, got {band:?}"
    );

    // Completion sheds each row; the band vanishes with the last one.
    model.route_raw(&raw_task(
        3,
        60_000,
        &completed(
            "task-a",
            "server",
            TaskTerminalState::Completed { exit_code: Some(0) },
            "",
        ),
    ));
    model.route_raw(&raw_task(
        4,
        61_000,
        &completed("task-b", "watcher", TaskTerminalState::Killed, ""),
    ));
    let (rows, _) = draw(&model, 100, 30);
    assert!(
        !rows.iter().any(|row| row.contains("background task")),
        "terminal tasks leave the band entirely"
    );
    assert!(
        !model.animated(),
        "terminal tasks close the anim gate again"
    );
}

/// MUTATION CHECK (plain parity): route the task notes around the shared
/// projection note voice. Expected RUNTIME failure: plain mode loses the
/// started/completion lines the TUI paints.
#[test]
fn plain_mode_prints_the_same_task_lines() {
    let mut model = live_session();
    model.route_raw(&raw_task(
        1,
        10_000,
        &started("task-a", "build", "cargo build", 10_000),
    ));
    model.route_raw(&raw_task(
        2,
        52_000,
        &completed(
            "task-a",
            "build",
            TaskTerminalState::Failed {
                reason: "orphaned".into(),
            },
            "error: linking failed\n",
        ),
    ));
    let plain = haider_tui::plain::render_plain(&model.projection, 200, None);
    assert!(
        plain.contains("⚙ task started — build · cargo build"),
        "plain prints the started line: {plain}"
    );
    assert!(
        plain.contains("└ task build — failed · 42s — error: linking failed"),
        "plain prints the completion line with exit + tail: {plain}"
    );
}

/// MUTATION CHECK (replay honesty): drop the completed-without-started
/// fallback. Expected RUNTIME failure: a completion whose started fact sits
/// outside the replayed window paints no terminal row and the band logic
/// misses the task entirely.
#[test]
fn completion_without_started_still_lands_a_terminal_row() {
    let mut model = live_session();
    model.route_raw(&raw_task(
        1,
        52_000,
        &completed(
            "task-orphan",
            "ghost",
            TaskTerminalState::Completed { exit_code: Some(2) },
            "",
        ),
    ));
    assert_eq!(model.tasks.running_count(), 0);
    let row = model
        .tasks
        .rows()
        .iter()
        .find(|row| row.task == "task-orphan")
        .expect("terminal row lands");
    assert_eq!(
        row.state,
        haider_tui::taskrows::TaskRowState::Completed { exit_code: Some(2) }
    );
    let notes = note_texts(&model);
    assert_eq!(notes.last().expect("note"), "└ task ghost — exit 2 · 42s");
}
