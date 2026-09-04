//! D4 — the generic `plan` tool presents and proceeds: the actor journals a
//! durable `origin: "plan"` document and its automatic acceptance, then
//! returns `{decision, note}` without parking for input.
#![allow(clippy::expect_used)]

use haider_core::{HarnessActor, HarnessConfig, MemoryStore, SubmitTurn};
use haider_protocol::EventPayload;
use haider_protocol::ids::{DeviceId, SessionId};
use haider_protocol::menu::{AnswerVia, MenuKind};
use haider_protocol::provider::FinishReason;
use haider_protocol::state::RunState;
use haider_provider::{FakeProvider, FakeStep};
use std::sync::Arc;
use std::time::Duration;

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

/// MUTATION CHECK: park on `InputRequired`, omit the automatic answer, drop
/// the markdown body, lose the `plan` origin, or change the accepted result.
/// Expected RUNTIME failure.
#[tokio::test]
async fn plan_journals_the_document_and_auto_accepts_without_parking() {
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
    let outcome = tokio::time::timeout(Duration::from_secs(1), turn.wait())
        .await
        .expect("autonomous plan must not wait for input")
        .expect("turn completes");
    assert_eq!(outcome.state, RunState::Done);

    let events = store.events(&SessionId::new(SESSION)).await;
    let payloads: Vec<EventPayload> = events
        .iter()
        .map(|event| serde_json::from_value(event.payload.clone().into()).expect("typed payload"))
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
    assert!(!opened[0].blocking);
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
        ["accept"]
    );
    let answered = payloads
        .iter()
        .find_map(|payload| match payload {
            EventPayload::MenuAnswered(answer) => Some(answer),
            _ => None,
        })
        .expect("plan auto-acceptance journaled");
    assert_eq!(answered.menu, opened[0].id);
    assert_eq!(answered.option_key.as_deref(), Some("accept"));
    assert_eq!(answered.option_index, 0);
    assert_eq!(answered.via, AnswerVia::Hook);

    assert!(payloads.iter().all(|payload| !matches!(
        payload,
        EventPayload::RunState(RunState::InputRequired { .. })
    )));

    // The result the model sees is the fixed autonomous acceptance.
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
    assert_eq!(parsed["note"], "");
    // Presentation, never a brokered effect.
    assert!(
        payloads
            .iter()
            .all(|payload| !matches!(payload, EventPayload::Effect(_)))
    );

    let opened_index = payloads
        .iter()
        .position(|payload| matches!(payload, EventPayload::MenuOpened(_)))
        .expect("opening position");
    let answered_index = payloads
        .iter()
        .position(|payload| matches!(payload, EventPayload::MenuAnswered(_)))
        .expect("answer position");
    let result_index = payloads
        .iter()
        .position(|payload| matches!(payload, EventPayload::ToolResult { .. }))
        .expect("result position");
    assert!(opened_index < answered_index && answered_index < result_index);
}
