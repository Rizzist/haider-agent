#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::ids::{MenuId, TaskId};
use haider_protocol::menu::{
    AnswerVia, DecisionKind, Menu, MenuAnswer, MenuKind, MenuOption, MenuScope,
};
use haider_protocol::task::{
    TaskCompleted, TaskCompletionDelivery, TaskEventPayload, TaskTerminalState,
};
use haider_tui::app::{AppModel, ChipModel, ChipQuestion, Hit, InboxKind, RuntimeMode, Screen};
use haider_tui::projection::{SessionProjection, TranscriptEntry};
use haider_tui::script::{ChipDisplayState, ChipSeed};
use ratatui::crossterm::event::KeyCode;

mod common;
mod tuivirt_common;
use common::{ctrl, key, run_slash};
use tuivirt_common::{apply, draw, file_review_menu, session_model};

fn review_model() -> AppModel {
    let mut model = session_model();
    let menu = file_review_menu();
    let id = menu.id.clone();
    apply(&mut model, EventPayload::MenuOpened(menu));
    apply(
        &mut model,
        EventPayload::RunState(haider_protocol::state::RunState::PermissionRequired { menu: id }),
    );
    model
}

fn question_menu(id: &str, title: &str) -> Menu {
    Menu {
        id: MenuId::new(id),
        kind: MenuKind::Question,
        title: title.to_owned(),
        body: vec!["Choose the next step.".to_owned()],
        options: vec![MenuOption {
            key: "continue".to_owned(),
            label: "Continue".to_owned(),
            detail: None,
            decision: None,
        }],
        blocking: true,
        scope: MenuScope::Session,
        origin: "agent".to_owned(),
        ttl_ms: None,
        timeout_option: None,
    }
}

#[test]
fn diff_review_opens_navigates_hunks_and_uses_existing_decision_path() {
    let mut model = review_model();
    let plain = haider_tui::plain::render_plain(&model.projection, 0, None);
    assert!(plain.contains("file review:"));
    assert!(plain.contains("removal; old 119; new -"));
    assert!(plain.contains("addition; old -; new 119"));
    let first = draw(&model, 118, 36);
    assert!(first.contains("EDIT REVIEW"));
    assert!(first.contains("hunk 1/2"));
    assert!(first.contains("@@ -118,3 +118,4 @@"));
    assert!(first.contains("119"), "numbered diff: {:?}", first.rows);

    model.handle(key(KeyCode::Right));
    let second = draw(&model, 118, 36);
    assert!(second.contains("hunk 2/2"));
    assert!(second.contains("@@ -240,1 +241,1 @@"));

    model.handle(key(KeyCode::Tab));
    model.handle(key(KeyCode::Enter));
    let answer = model.outbox.pop().expect("reject answer").answer;
    assert_eq!(answer.option_key.as_deref(), Some("deny"));
    let menu = model
        .projection
        .open_menu()
        .expect("menu remains until echo");
    assert_eq!(
        menu.options[answer.option_index as usize].decision,
        Some(DecisionKind::RejectOnce)
    );

    let mut accept = review_model();
    accept.handle(key(KeyCode::Char('1')));
    let answer = accept.outbox.pop().expect("allow answer").answer;
    assert_eq!(answer.option_key.as_deref(), Some("allow"));
    assert_eq!(
        accept
            .projection
            .open_menu()
            .expect("open until echo")
            .options[answer.option_index as usize]
            .decision,
        Some(DecisionKind::AllowOnce)
    );
}

#[test]
fn file_review_and_answer_replay_to_the_same_visible_decision() {
    let source = review_model();
    let menu = source.projection.open_menu().expect("review menu").clone();
    let opened: EventPayload = serde_json::from_str(
        &serde_json::to_string(&EventPayload::MenuOpened(menu.clone())).expect("serialize open"),
    )
    .expect("replay open");
    let answer = MenuAnswer {
        menu: menu.id.clone(),
        option_key: Some("allow".to_owned()),
        option_index: 0,
        value: None,
        via: AnswerVia::Rpc,
    };
    let answered: EventPayload = serde_json::from_str(
        &serde_json::to_string(&EventPayload::MenuAnswered(answer)).expect("serialize answer"),
    )
    .expect("replay answer");
    let mut projection = SessionProjection::new();
    projection.apply(&opened);
    assert!(matches!(
        projection.open_menu().map(|menu| &menu.kind),
        Some(MenuKind::Permission {
            file_review: Some(_),
            ..
        })
    ));
    projection.apply(&answered);
    assert!(projection.open_menu().is_none());
    assert!(projection.entries().iter().any(|entry| matches!(
        entry,
        TranscriptEntry::Note { text } if text.contains("permission allowed once")
    )));
}

#[test]
fn inbox_aggregates_asks_agent_questions_and_stalled_tasks_without_new_state() {
    let mut model = review_model();
    let mut chip = ChipModel::from_seed(ChipSeed {
        agent: "agent-review".to_owned(),
        parent: None,
        ros: None,
        callsign: "Husayn".to_owned(),
        hon: "(r)",
        full: "Husayn ibn Ali".to_owned(),
        name: "review migration".to_owned(),
        model: "fable-5".to_owned(),
        device: "local".to_owned(),
        state: ChipDisplayState::InputRequired,
        tokens: 10,
        prefill: Vec::new(),
    });
    chip.question = Some(ChipQuestion {
        recovery: false,
        text: "Which migration?".to_owned(),
        options: vec!["Continue".to_owned()],
        resolved: false,
    });
    chip.transcript
        .apply(&EventPayload::MenuOpened(question_menu(
            "agent-question",
            "Agent needs a migration choice",
        )));
    model.chips.push(chip);
    model
        .tasks
        .apply(&TaskEventPayload::TaskCompleted(TaskCompleted {
            completion_consumer: None,
            task: TaskId::new("task-failed"),
            name: "cargo test".to_owned(),
            state: TaskTerminalState::Failed {
                reason: "test process disappeared".to_owned(),
            },
            elapsed_ms: 900,
            output_bytes: 0,
            output_sha256: None,
            tail: String::new(),
            output_digest: None,
            output_digest_complete: false,
            artifact: None,
            full_output_unavailable: false,
            truncated: false,
            delivery: TaskCompletionDelivery::DeliveredQueued,
            workspace_mutation: None,
        }));

    let rows = model.inbox_rows();
    assert_eq!(
        rows.iter().filter(|row| row.kind == InboxKind::Ask).count(),
        1
    );
    assert_eq!(
        rows.iter()
            .filter(|row| row.kind == InboxKind::Agent)
            .count(),
        1
    );
    assert_eq!(
        rows.iter()
            .filter(|row| row.kind == InboxKind::StalledTask)
            .count(),
        1
    );
    let frame = draw(&model, 118, 36);
    assert!(frame.contains("Needs you"));
    assert!(frame.has_hit(|hit| matches!(hit, Hit::InboxBand)));
    model.handle_hit(Hit::InboxBand);
    let inbox = draw(&model, 118, 36);
    assert!(inbox.contains("NEEDS YOU"));
    assert!(inbox.contains("Agent needs a migration choice"));
    assert!(inbox.contains("cargo test"));
}

#[test]
fn inbox_command_opens_and_escape_returns_to_the_current_surface() {
    let mut model = AppModel::new();
    model.mode = RuntimeMode::Live;
    model.screen = Screen::Session;
    run_slash(&mut model, "/inbox");
    assert!(model.inbox_open);
    model.handle(key(KeyCode::Esc));
    assert!(!model.inbox_open);
    assert_eq!(model.screen, Screen::Session);

    run_slash(&mut model, "/inbox");
    model.handle(ctrl(KeyCode::Char('c')));
    assert!(!model.inbox_open);
    assert_eq!(model.screen, Screen::Launcher);
}
