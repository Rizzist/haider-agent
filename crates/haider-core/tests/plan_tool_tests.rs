//! D4 — the generic `plan` tool rides the request_input park/answer/resume
//! machinery: the proposal parks the run on a durable `origin: "plan"` menu
//! carrying the full markdown body, and the committed decision returns to the
//! model as `{decision, note}`.
#![allow(clippy::expect_used)]

use haider_core::{HarnessActor, HarnessConfig, MemoryStore, SubmitTurn};
use haider_protocol::EventPayload;
use haider_protocol::ids::{DeviceId, SessionId};
use haider_protocol::menu::{AnswerVia, MenuAnswer, MenuKind};
use haider_protocol::provider::FinishReason;
use haider_protocol::state::RunState;
use haider_provider::{FakeProvider, FakeStep};
use std::sync::Arc;

const SESSION: &str = "plan-tool-session";

fn actor(
    script: Vec<FakeStep>,
) -> (
    haider_core::HarnessHandle,
    Arc<MemoryStore>,
    Arc<FakeProvider>,
) {
    let config = HarnessConfig::for_session(
        SessionId::new(SESSION),
        DeviceId::new("plan-tool-device"),
        7,
        11,
    )
    .with_started_at_ms(1_700_000_000_000);
    let store = Arc::new(MemoryStore::new());
    let provider = Arc::new(FakeProvider::new(script));
    let handle = HarnessActor::spawn(config, provider.clone(), store.clone());
    (handle, store, provider)
}

fn plan_call(call_id: &str) -> FakeStep {
    FakeStep::EmitToolCall {
        call_id: call_id.into(),
        name: "plan".into(),
        args: serde_json::json!({
            "title": "Datacenter build-out",
            "body": "# Tiers\n\n- edge pops\n- core compute\n\n## Cost\n\n$4M/yr",
        }),
    }
}

/// MUTATION CHECK: stop parking on InputRequired, drop the markdown body from
/// the durable menu, lose the `plan` origin, or change the result shape.
/// Expected RUNTIME failure below.
#[tokio::test]
async fn plan_parks_on_a_durable_menu_and_returns_the_decision() {
    let (handle, store, _provider) = actor(vec![
        plan_call("plan-1"),
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "plan-1".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let turn = handle
        .submit_turn(SubmitTurn::new("propose the datacenter"))
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
            option_key: Some("accept".into()),
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
    assert_eq!(opened[0].origin, "plan");
    assert_eq!(opened[0].kind, MenuKind::Choice);
    assert_eq!(opened[0].title, "Datacenter build-out");
    // The full markdown document rides the durable menu body, line by line —
    // that is what clients render full screen and what a restart reconstructs.
    assert_eq!(opened[0].body[0], "# Tiers");
    assert!(opened[0].body.iter().any(|line| line == "$4M/yr"));
    assert_eq!(
        opened[0]
            .options
            .iter()
            .map(|option| option.key.as_str())
            .collect::<Vec<_>>(),
        ["accept", "revise", "reject"]
    );
    // The result the model sees is {decision, note}.
    let result = payloads
        .iter()
        .find_map(|payload| match payload {
            EventPayload::ToolResult { call_id, result } if call_id == "plan-1" => {
                Some(result.preview.clone())
            }
            _ => None,
        })
        .expect("plan tool result journaled");
    let parsed: serde_json::Value = serde_json::from_str(&result).expect("json result");
    assert_eq!(parsed["decision"], "accept");
    assert!(parsed["note"].is_null());
    // Interaction, never a brokered effect.
    assert!(
        payloads
            .iter()
            .all(|payload| !matches!(payload, EventPayload::Effect(_)))
    );
}

/// MUTATION CHECK: drop the revise-note pass-through, or reject the value
/// accompanying an option answer. Expected RUNTIME failure below.
#[tokio::test]
async fn revise_decision_carries_the_note_back_to_the_model() {
    let (handle, store, _provider) = actor(vec![
        plan_call("plan-2"),
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "plan-2".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let turn = handle
        .submit_turn(SubmitTurn::new("propose it"))
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
            menu,
            option_key: Some("revise".into()),
            option_index: 1,
            value: Some("cut the cost section in half".into()),
            via: AnswerVia::Rpc,
        })
        .await
        .expect("answer accepted");
    let outcome = turn.wait().await.expect("turn completes");
    assert_eq!(outcome.state, RunState::Done);

    let events = store.events(&SessionId::new(SESSION)).await;
    let result = events
        .iter()
        .filter_map(|event| serde_json::from_value::<EventPayload>(event.payload.clone()).ok())
        .find_map(|payload| match payload {
            EventPayload::ToolResult { call_id, result } if call_id == "plan-2" => {
                Some(result.preview)
            }
            _ => None,
        })
        .expect("plan tool result journaled");
    let parsed: serde_json::Value = serde_json::from_str(&result).expect("json result");
    assert_eq!(parsed["decision"], "revise");
    assert_eq!(parsed["note"], "cut the cost section in half");
}
