#![allow(clippy::expect_used)]

use haider_core::{HarnessActor, HarnessConfig, MemoryStore, SubmitTurn};
use haider_protocol::EventPayload;
use haider_protocol::ids::{DeviceId, SessionId};
use haider_protocol::item::{ItemEvent, ToolStatus, TurnItem};
use haider_protocol::menu::{AnswerVia, MenuAnswer, MenuKind};
use haider_protocol::provider::FinishReason;
use haider_protocol::state::RunState;
use haider_provider::{FakeInputKind, FakeInputOption, FakeProvider, FakeStep};
use std::sync::Arc;

const SESSION: &str = "request-input-session";

fn actor(script: Vec<FakeStep>) -> (haider_core::HarnessHandle, Arc<MemoryStore>) {
    let config = HarnessConfig::for_session(
        SessionId::new(SESSION),
        DeviceId::new("request-input-device"),
        7,
        11,
    )
    .with_started_at_ms(1_700_000_000_000);
    let store = Arc::new(MemoryStore::new());
    let handle = HarnessActor::spawn(config, Arc::new(FakeProvider::new(script)), store.clone());
    (handle, store)
}

#[tokio::test]
async fn request_input_journals_menu_round_trip_and_returns_answer_as_tool_result() {
    let (handle, store) = actor(vec![
        FakeStep::EmitRequestInput {
            call_id: "question-1".into(),
            kind: FakeInputKind::Choice,
            title: "Choose a target".into(),
            body: vec!["The server owns this exact list.".into()],
            options: vec![
                FakeInputOption {
                    key: "library".into(),
                    label: "Library".into(),
                    detail: None,
                },
                FakeInputOption {
                    key: "binary".into(),
                    label: "Binary".into(),
                    detail: Some("Creates a CLI".into()),
                },
            ],
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let turn = handle
        .submit_turn(SubmitTurn::new("ask before choosing"))
        .await
        .expect("turn accepted");
    let mut state = handle.state_receiver();
    let parked = state
        .wait_for(|state| matches!(state, Some(RunState::InputRequired { .. })))
        .await
        .expect("actor stays available")
        .clone();
    let RunState::InputRequired { menu } = parked.expect("state exists") else {
        panic!("wait predicate guarantees InputRequired");
    };
    handle
        .answer_menu(MenuAnswer {
            menu: menu.clone(),
            option_key: Some("binary".into()),
            option_index: 0,
            value: None,
            via: AnswerVia::Rpc,
        })
        .await
        .expect("answer accepted");
    let outcome = turn.wait().await.expect("turn completes");
    assert_eq!(outcome.state, RunState::Done);

    let events = store.events(&SessionId::new(SESSION)).await;
    let payloads: Vec<EventPayload> = events
        .iter()
        .map(|event| serde_json::from_value(event.payload.clone()).expect("typed payload"))
        .collect();
    let opened: Vec<_> = payloads
        .iter()
        .filter_map(|payload| match payload {
            EventPayload::MenuOpened(menu) => Some(menu),
            _ => None,
        })
        .collect();
    assert_eq!(opened.len(), 1);
    assert_eq!(opened[0].id, menu);
    assert_eq!(opened[0].kind, MenuKind::Choice);
    assert_eq!(
        opened[0]
            .options
            .iter()
            .map(|option| (option.key.as_str(), option.label.as_str()))
            .collect::<Vec<_>>(),
        [("library", "Library"), ("binary", "Binary")]
    );
    assert!(
        opened[0]
            .options
            .iter()
            .all(|option| option.decision.is_none())
    );
    assert_eq!(
        payloads
            .iter()
            .filter(|payload| matches!(payload, EventPayload::MenuAnswered(_)))
            .count(),
        1
    );
    assert!(
        payloads
            .iter()
            .all(|payload| !matches!(payload, EventPayload::Effect(_))),
        "request_input is journaled as interaction, not permission/effect"
    );
    let tool_result = payloads.iter().find_map(|payload| match payload {
        EventPayload::ToolResult { call_id, result } if call_id == "question-1" => Some(result),
        _ => None,
    });
    let tool_result = tool_result.expect("answer returned as tool result");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&tool_result.preview).expect("answer JSON"),
        serde_json::json!({"value":"Binary","option_key":"binary"})
    );
    assert!(payloads.iter().any(|payload| matches!(
        payload,
        EventPayload::Item(ItemEvent::Completed {
            item: TurnItem::ToolCall {
                call_id,
                status: ToolStatus::Completed,
                ..
            },
            ..
        }) if call_id == "question-1"
    )));

    let opened_index = payloads
        .iter()
        .position(|payload| matches!(payload, EventPayload::MenuOpened(_)))
        .expect("opened");
    let parked_index = payloads
        .iter()
        .position(|payload| {
            matches!(
                payload,
                EventPayload::RunState(RunState::InputRequired { .. })
            )
        })
        .expect("parked");
    let answered_index = payloads
        .iter()
        .position(|payload| matches!(payload, EventPayload::MenuAnswered(_)))
        .expect("answered");
    let result_index = payloads
        .iter()
        .position(|payload| matches!(payload, EventPayload::ToolResult { .. }))
        .expect("result");
    assert!(opened_index < parked_index);
    assert!(parked_index < answered_index);
    assert!(answered_index < result_index);
}

#[tokio::test]
async fn invalid_choice_keeps_the_menu_open_for_a_later_valid_answer() {
    let (handle, _store) = actor(vec![
        FakeStep::EmitRequestInput {
            call_id: "question-2".into(),
            kind: FakeInputKind::Choice,
            title: "Continue?".into(),
            body: Vec::new(),
            options: vec![FakeInputOption {
                key: "yes".into(),
                label: "Yes".into(),
                detail: None,
            }],
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let turn = handle
        .submit_turn(SubmitTurn::new("ask"))
        .await
        .expect("turn accepted");
    let mut state = handle.state_receiver();
    let parked = state
        .wait_for(|state| matches!(state, Some(RunState::InputRequired { .. })))
        .await
        .expect("parked")
        .clone();
    let RunState::InputRequired { menu } = parked.expect("state") else {
        panic!("InputRequired");
    };
    let invalid = handle
        .answer_menu(MenuAnswer {
            menu: menu.clone(),
            option_key: Some("client-invented".into()),
            option_index: 99,
            value: None,
            via: AnswerVia::Rpc,
        })
        .await
        .expect_err("invented option rejected");
    assert_eq!(
        invalid.code,
        haider_protocol::error::ErrorCode::InvalidArgument
    );
    assert_eq!(
        handle.current_state(),
        Some(RunState::InputRequired { menu: menu.clone() })
    );
    handle
        .answer_menu(MenuAnswer {
            menu,
            option_key: Some("yes".into()),
            option_index: 0,
            value: None,
            via: AnswerVia::Rpc,
        })
        .await
        .expect("valid answer accepted");
    assert_eq!(turn.wait().await.expect("outcome").state, RunState::Done);
}

#[tokio::test]
async fn free_form_question_requires_and_returns_the_typed_value() {
    let (handle, store) = actor(vec![
        FakeStep::EmitRequestInput {
            call_id: "question-3".into(),
            kind: FakeInputKind::Question,
            title: "What should the module be called?".into(),
            body: Vec::new(),
            options: Vec::new(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let turn = handle
        .submit_turn(SubmitTurn::new("ask for a name"))
        .await
        .expect("turn accepted");
    let mut state = handle.state_receiver();
    let parked = state
        .wait_for(|state| matches!(state, Some(RunState::InputRequired { .. })))
        .await
        .expect("parked")
        .clone();
    let RunState::InputRequired { menu } = parked.expect("state") else {
        panic!("InputRequired");
    };
    handle
        .answer_menu(MenuAnswer {
            menu,
            option_key: None,
            option_index: 0,
            value: Some("process_runtime".into()),
            via: AnswerVia::Rpc,
        })
        .await
        .expect("free-form answer accepted");
    assert_eq!(turn.wait().await.expect("outcome").state, RunState::Done);

    let payloads: Vec<EventPayload> = store
        .events(&SessionId::new(SESSION))
        .await
        .iter()
        .map(|event| serde_json::from_value(event.payload.clone()).expect("typed payload"))
        .collect();
    assert!(payloads.iter().any(|payload| matches!(
        payload,
        EventPayload::MenuOpened(menu) if menu.kind == MenuKind::Question && menu.options.is_empty()
    )));
    let result = payloads.iter().find_map(|payload| match payload {
        EventPayload::ToolResult { result, .. } => Some(&result.preview),
        _ => None,
    });
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(result.expect("tool result"))
            .expect("answer JSON"),
        serde_json::json!({"value":"process_runtime","option_key":null})
    );
}

#[tokio::test]
async fn cancellation_while_input_is_required_closes_the_tool_as_cancelled() {
    let (handle, store) = actor(vec![
        FakeStep::EmitRequestInput {
            call_id: "question-4".into(),
            kind: FakeInputKind::Choice,
            title: "Wait here".into(),
            body: Vec::new(),
            options: vec![FakeInputOption {
                key: "continue".into(),
                label: "Continue".into(),
                detail: None,
            }],
        },
        FakeStep::Hang,
    ]);
    let turn = handle
        .submit_turn(SubmitTurn::new("ask and cancel"))
        .await
        .expect("turn accepted");
    let mut state = handle.state_receiver();
    state
        .wait_for(|state| matches!(state, Some(RunState::InputRequired { .. })))
        .await
        .expect("parked");
    turn.cancel();
    assert_eq!(
        turn.wait().await.expect("outcome").state,
        RunState::Cancelled
    );

    let payloads: Vec<EventPayload> = store
        .events(&SessionId::new(SESSION))
        .await
        .iter()
        .map(|event| serde_json::from_value(event.payload.clone()).expect("typed payload"))
        .collect();
    assert!(
        payloads
            .iter()
            .all(|payload| !matches!(payload, EventPayload::MenuAnswered(_)))
    );
    assert!(payloads.iter().any(|payload| matches!(
        payload,
        EventPayload::Item(ItemEvent::Completed {
            item: TurnItem::ToolCall {
                call_id,
                status: ToolStatus::Cancelled,
                ..
            },
            ..
        }) if call_id == "question-4"
    )));
}
