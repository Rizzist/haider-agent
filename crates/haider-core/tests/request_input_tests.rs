#![allow(clippy::expect_used)]

use haider_core::{HarnessActor, HarnessConfig, MemoryStore, StoreHandle, SubmitTurn};
use haider_protocol::EventPayload;
use haider_protocol::error::ErrorCode;
use haider_protocol::ids::{BranchId, DeviceId, EventId, SessionId};
use haider_protocol::item::{ItemEvent, ToolStatus, TurnItem};
use haider_protocol::menu::{AnswerVia, MenuAnswer, MenuKind};
use haider_protocol::provider::FinishReason;
use haider_protocol::state::RunState;
use haider_provider::{FakeInputKind, FakeInputOption, FakeProvider, FakeStep};
use std::sync::Arc;

const SESSION: &str = "request-input-session";

fn actor(
    script: Vec<FakeStep>,
) -> (
    haider_core::HarnessHandle,
    Arc<MemoryStore>,
    Arc<FakeProvider>,
) {
    let config = HarnessConfig::for_session(
        SessionId::new(SESSION),
        DeviceId::new("request-input-device"),
        7,
        11,
    )
    .with_started_at_ms(1_700_000_000_000);
    let store = Arc::new(MemoryStore::new());
    let provider = Arc::new(FakeProvider::new(script));
    let handle = HarnessActor::spawn(config, provider.clone(), store.clone());
    (handle, store, provider)
}

/// MUTATION CHECK: omit `HarnessConfig::branch_id` from menu/tool/item/run
/// envelope construction. Expected RUNTIME failure: a request-input lifecycle
/// event below lands on main instead of its accepted branch.
#[tokio::test]
async fn branch_scoped_request_input_keeps_every_interaction_on_its_branch() {
    let branch_id = BranchId::new("request-input-branch");
    let mut config = HarnessConfig::for_session(
        SessionId::new(SESSION),
        DeviceId::new("request-input-branch-device"),
        7,
        11,
    );
    config.branch_id = Some(branch_id.clone());
    let store = Arc::new(MemoryStore::new());
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitRequestInput {
            call_id: "branch-question".into(),
            kind: FakeInputKind::Choice,
            title: "Choose".into(),
            body: Vec::new(),
            options: vec![FakeInputOption {
                key: "yes".into(),
                label: "Yes".into(),
                detail: None,
            }],
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "branch-question".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let handle = HarnessActor::spawn(config, provider, store.clone());
    let turn = handle
        .submit_turn(SubmitTurn::new("ask on branch"))
        .await
        .expect("turn accepted");
    let mut state = handle.state_receiver();
    let parked = state
        .wait_for(|state| matches!(state, Some(RunState::InputRequired { .. })))
        .await
        .expect("actor stays available")
        .clone();
    let RunState::InputRequired { menu } = parked.expect("input state") else {
        panic!("wait predicate guarantees input state");
    };
    handle
        .answer_menu(MenuAnswer {
            menu,
            option_key: Some("yes".into()),
            option_index: 0,
            value: None,
            via: AnswerVia::Rpc,
        })
        .await
        .expect("answer menu");
    assert_eq!(
        turn.wait().await.expect("turn completes").state,
        RunState::Done
    );
    let events = store.events(&SessionId::new(SESSION)).await;
    assert!(!events.is_empty());
    assert!(
        events
            .iter()
            .all(|event| event.branch_id.as_ref() == Some(&branch_id))
    );
    let payloads = events
        .iter()
        .map(|event| {
            serde_json::from_value::<EventPayload>(event.payload.clone()).expect("typed payload")
        })
        .collect::<Vec<_>>();
    assert!(
        payloads
            .iter()
            .any(|payload| matches!(payload, EventPayload::MenuOpened(_)))
    );
    assert!(
        payloads
            .iter()
            .any(|payload| matches!(payload, EventPayload::MenuAnswered(_)))
    );
    assert!(payloads.iter().any(|payload| matches!(
        payload,
        EventPayload::Item(ItemEvent::Started {
            item: TurnItem::ToolCall { .. },
            ..
        })
    )));
    assert!(
        payloads
            .iter()
            .any(|payload| matches!(payload, EventPayload::ToolResult { .. }))
    );
    assert!(
        payloads
            .iter()
            .any(|payload| matches!(payload, EventPayload::RunState(RunState::Done)))
    );
}

#[tokio::test]
async fn request_input_journals_menu_round_trip_and_returns_answer_as_tool_result() {
    let (handle, store, provider) = actor(vec![
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
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "question-1".into(),
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

    let requests = provider.requests();
    assert_eq!(requests.len(), 2, "tool answer requires a second request");
    let result = requests[1]
        .messages
        .iter()
        .find_map(|message| message.tool_result_for("question-1"))
        .expect("request N+1 contains the request_input answer");
    assert!(matches!(
        result,
        haider_protocol::provider::Block::ToolResult { preview, .. }
            if serde_json::from_str::<serde_json::Value>(preview).expect("answer JSON")
                == serde_json::json!({"value":"Binary","option_key":"binary"})
    ));
}

/// MUTATION CHECK: route an externally committed answer back through
/// `answer_menu`, causing the harness to append a second `MenuAnswered`, or
/// omit the committed-event wake. Expected failure: the turn hangs or the
/// durable history contains two resolutions.
#[tokio::test]
async fn committed_menu_event_wakes_waiter_without_reappending_resolution() {
    let (handle, store, _provider) = actor(vec![
        FakeStep::EmitRequestInput {
            call_id: "durable-question".into(),
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
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "durable-question".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let turn = handle
        .submit_turn(SubmitTurn::new("ask durably"))
        .await
        .expect("turn accepted");
    let mut state = handle.state_receiver();
    let parked = state
        .wait_for(|state| matches!(state, Some(RunState::InputRequired { .. })))
        .await
        .expect("actor parks")
        .clone();
    let Some(RunState::InputRequired { menu }) = parked else {
        panic!("wait predicate guarantees InputRequired");
    };

    let opening = store
        .events(&SessionId::new(SESSION))
        .await
        .into_iter()
        .find(|envelope| {
            serde_json::from_value::<EventPayload>(envelope.payload.clone())
                .is_ok_and(|payload| matches!(payload, EventPayload::MenuOpened(_)))
        })
        .expect("menu opening is durable");
    let mut resolutions = [haider_protocol::envelope::EventEnvelope {
        event_id: EventId::new("external-menu-resolution"),
        seq: 0,
        committed_at_ms: 0,
        causation_id: Some(opening.event_id.clone()),
        payload: serde_json::to_value(EventPayload::MenuAnswered(MenuAnswer {
            menu,
            option_key: Some("yes".into()),
            option_index: 0,
            value: None,
            via: AnswerVia::Rpc,
        }))
        .expect("answer serializes"),
        ..opening
    }];
    store
        .append(&mut resolutions)
        .await
        .expect("external authority commits");
    handle
        .apply_committed_menu_event(resolutions[0].clone())
        .expect("committed event wakes waiter");

    assert_eq!(
        turn.wait().await.expect("turn completes").state,
        RunState::Done
    );
    let history = store.events(&SessionId::new(SESSION)).await;
    assert_eq!(
        history
            .iter()
            .filter(|envelope| {
                serde_json::from_value::<EventPayload>(envelope.payload.clone())
                    .is_ok_and(|payload| matches!(payload, EventPayload::MenuAnswered(_)))
            })
            .count(),
        1
    );
}

#[tokio::test]
async fn provider_request_ceiling_ends_the_same_turn_with_typed_loop_limit() {
    let mut config = HarnessConfig::for_session(
        SessionId::new(SESSION),
        DeviceId::new("request-input-device"),
        7,
        11,
    )
    .with_started_at_ms(1_700_000_000_000);
    config.max_provider_requests_per_turn = 1;
    let store = Arc::new(MemoryStore::new());
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitRequestInput {
            call_id: "loop-question".into(),
            kind: FakeInputKind::Question,
            title: "Continue the loop?".into(),
            body: Vec::new(),
            options: Vec::new(),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
    ]));
    let handle = HarnessActor::spawn(config, provider.clone(), store.clone());
    let turn = handle
        .submit_turn(SubmitTurn::new("enforce request ceiling"))
        .await
        .expect("turn accepted");
    let mut state = handle.state_receiver();
    let parked = state
        .wait_for(|state| matches!(state, Some(RunState::InputRequired { .. })))
        .await
        .expect("request_input parks")
        .clone();
    let Some(RunState::InputRequired { menu }) = parked else {
        panic!("wait predicate guarantees InputRequired");
    };
    handle
        .answer_menu(MenuAnswer {
            menu,
            option_key: None,
            option_index: 0,
            value: Some("yes".into()),
            via: AnswerVia::Rpc,
        })
        .await
        .expect("answer accepted");

    let outcome = turn.wait().await.expect("typed turn outcome");
    assert_eq!(outcome.state, RunState::Errored);
    let error = outcome.error.expect("loop-limit error");
    assert_eq!(error.code, ErrorCode::LoopLimit);
    assert_eq!(
        error.details.expect("loop-limit details"),
        serde_json::json!({
            "provider_request_count": 2,
            "provider_request_limit": 1,
        })
    );
    assert_eq!(
        provider.requests().len(),
        1,
        "the over-limit request is never sent"
    );
    let events = store.events(&SessionId::new(SESSION)).await;
    assert!(matches!(
        events
            .last()
            .map(|event| serde_json::from_value(event.payload.clone()).expect("payload")),
        Some(EventPayload::RunState(RunState::Errored))
    ));
}

#[tokio::test]
async fn invalid_choice_keeps_the_menu_open_for_a_later_valid_answer() {
    let (handle, _store, _provider) = actor(vec![
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
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "question-2".into(),
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
    let (handle, store, _provider) = actor(vec![
        FakeStep::EmitRequestInput {
            call_id: "question-3".into(),
            kind: FakeInputKind::Question,
            title: "What should the module be called?".into(),
            body: Vec::new(),
            options: Vec::new(),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "question-3".into(),
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
    let (handle, store, _provider) = actor(vec![
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
    let parked = state
        .wait_for(|state| matches!(state, Some(RunState::InputRequired { .. })))
        .await
        .expect("parked")
        .clone();
    let Some(RunState::InputRequired { menu }) = parked else {
        panic!("wait predicate guarantees InputRequired");
    };
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
        EventPayload::MenuClosed {
            menu: closed,
            reason: haider_protocol::menu::MenuCloseReason::Cancelled,
        } if closed == &menu
    )));
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

#[tokio::test]
async fn losing_menu_answer_is_rejected_while_the_followup_provider_hangs() {
    let (handle, _store, provider) = actor(vec![
        FakeStep::EmitRequestInput {
            call_id: "question-5".into(),
            kind: FakeInputKind::Choice,
            title: "Pick once".into(),
            body: Vec::new(),
            options: vec![FakeInputOption {
                key: "only".into(),
                label: "Only".into(),
                detail: None,
            }],
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "question-5".into(),
        },
        FakeStep::Hang,
    ]);
    let turn = handle
        .submit_turn(SubmitTurn::new("ask once"))
        .await
        .expect("turn accepted");
    let mut state = handle.state_receiver();
    let parked = state
        .wait_for(|state| matches!(state, Some(RunState::InputRequired { .. })))
        .await
        .expect("parked")
        .clone();
    let Some(RunState::InputRequired { menu }) = parked else {
        panic!("wait predicate guarantees InputRequired");
    };
    let answer = MenuAnswer {
        menu,
        option_key: Some("only".into()),
        option_index: 0,
        value: None,
        via: AnswerVia::Rpc,
    };
    handle
        .answer_menu(answer.clone())
        .await
        .expect("first surface wins");
    while provider.requests().len() < 2 {
        tokio::task::yield_now().await;
    }
    let stale = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        handle.answer_menu(answer),
    )
    .await
    .expect("stale answer must not hang")
    .expect_err("stale answer is rejected");
    assert_eq!(stale.code, haider_protocol::error::ErrorCode::MenuNotFound);
    turn.cancel();
    assert_eq!(
        turn.wait().await.expect("cancelled followup").state,
        RunState::Cancelled
    );
}
