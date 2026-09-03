//! G1 runtime laws for the actor-owned `todo_write` plan lifecycle: fact
//! emission (one item id, replace semantics), the Todos node commit, typed
//! rejection of invalid lists, and the empty-list no-op/clear split.
#![allow(clippy::expect_used)]

use haider_core::{HarnessActor, HarnessConfig, MemoryStore, SubmitTurn};
use haider_protocol::EventPayload;
use haider_protocol::envelope::PromptRender;
use haider_protocol::history::{NodeKind, TodoState};
use haider_protocol::ids::{DeviceId, SessionId};
use haider_protocol::item::{ItemEvent, ToolStatus, TurnItem};
use haider_protocol::provider::FinishReason;
use haider_protocol::state::RunState;
use haider_provider::{FakeProvider, FakeStep};
use std::sync::Arc;

const SESSION: &str = "todo-write-session";

fn actor(script: Vec<FakeStep>) -> (haider_core::HarnessHandle, Arc<MemoryStore>) {
    let config = HarnessConfig::for_session(
        SessionId::new(SESSION),
        DeviceId::new("todo-write-device"),
        7,
        11,
    )
    .with_started_at_ms(1_700_000_000_000);
    let store = Arc::new(MemoryStore::new());
    let provider = Arc::new(FakeProvider::new(script));
    let handle = HarnessActor::spawn(config, provider, store.clone());
    (handle, store)
}

fn todo_call(call_id: &str, items: serde_json::Value) -> [FakeStep; 3] {
    [
        FakeStep::EmitToolCall {
            call_id: call_id.into(),
            name: "todo_write".into(),
            args: serde_json::json!({ "items": items }),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: call_id.into(),
        },
    ]
}

async fn run_to_done(handle: &haider_core::HarnessHandle, text: &str) {
    let turn = handle
        .submit_turn(SubmitTurn::new(text))
        .await
        .expect("turn accepted");
    assert_eq!(
        turn.wait().await.expect("turn completes").state,
        RunState::Done
    );
}

async fn typed_payloads(store: &MemoryStore) -> Vec<(EventPayload, PromptRender)> {
    store
        .events(&SessionId::new(SESSION))
        .await
        .iter()
        .filter_map(|event| {
            serde_json::from_value::<EventPayload>(event.payload.clone().into())
                .ok()
                .map(|payload| (payload, event.render.prompt))
        })
        .collect()
}

fn plan_events(payloads: &[(EventPayload, PromptRender)]) -> Vec<(&ItemEvent, PromptRender)> {
    payloads
        .iter()
        .filter_map(|(payload, render)| match payload {
            EventPayload::Item(event)
                if matches!(
                    event,
                    ItemEvent::Started {
                        item: TurnItem::Plan { .. },
                        ..
                    } | ItemEvent::Completed {
                        item: TurnItem::Plan { .. },
                        ..
                    }
                ) =>
            {
                Some((event, *render))
            }
            _ => None,
        })
        .collect()
}

/// L3 (core seam): two `todo_write` calls in ONE run journal Started{Plan}
/// then Completed{Plan} under the SAME item id, the second list replacing the
/// first, with verbatim-render ToolResult facts answering each call.
/// MUTATION CHECK: mint a fresh item id per write (drop the `PlanLifecycle`
/// reuse). Expected RUNTIME failure: the Completed fact below stops sharing
/// the Started fact's item id.
#[tokio::test]
async fn two_writes_in_one_run_share_one_item_id_and_replace_the_list() {
    let mut script = Vec::new();
    script.extend(todo_call(
        "plan-1",
        serde_json::json!([
            { "id": 0, "text": "scope entrypoints", "state": "processing" },
            { "id": 1, "text": "patch run loop", "state": "listed", "dep": 0 },
        ]),
    ));
    script.extend(todo_call(
        "plan-2",
        serde_json::json!([
            { "id": 0, "text": "scope entrypoints", "state": "completed" },
            { "id": 1, "text": "patch run loop", "state": "processing", "dep": 0 },
        ]),
    ));
    script.extend([
        FakeStep::EmitText {
            text: "working through the plan".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let (handle, store) = actor(script);
    run_to_done(&handle, "plan the refactor").await;
    let payloads = typed_payloads(&store).await;
    let plans = plan_events(&payloads);
    assert_eq!(plans.len(), 2, "one Started, one Completed");
    let (
        ItemEvent::Started {
            item_id: started_id,
            item: TurnItem::Plan { items: first },
        },
        _,
    ) = plans[0]
    else {
        panic!("first plan fact must be Started, got {:?}", plans[0].0);
    };
    let (
        ItemEvent::Completed {
            item_id: completed_id,
            item: TurnItem::Plan { items: second },
        },
        _,
    ) = plans[1]
    else {
        panic!("second plan fact must be Completed, got {:?}", plans[1].0);
    };
    assert_eq!(started_id, completed_id, "one item id per plan lifecycle");
    assert_eq!(first[0].state, TodoState::Processing);
    assert_eq!(second[0].state, TodoState::Completed);
    assert_eq!(second[1].state, TodoState::Processing);
    assert_eq!(second[1].dep, Some(0));
    let tool_results: Vec<_> = payloads
        .iter()
        .filter_map(|(payload, render)| match payload {
            EventPayload::ToolResult { call_id, result } => Some((call_id, result, *render)),
            _ => None,
        })
        .collect();
    assert_eq!(tool_results.len(), 2);
    for (index, (call_id, result, render)) in tool_results.iter().enumerate() {
        assert_eq!(call_id.as_str(), format!("plan-{}", index + 1));
        assert_eq!(
            *render,
            PromptRender::Verbatim,
            "todo_write results replay into later prompts"
        );
        assert!(result.preview.contains("\"ok\":true"));
    }
    assert!(
        tool_results[1].1.preview.contains("\"completed\":1"),
        "second echo counts the completed item"
    );
    // Both todo_write ToolCall items settled Completed.
    let settled = payloads
        .iter()
        .filter(|(payload, _)| {
            matches!(
                payload,
                EventPayload::Item(ItemEvent::Completed {
                    item: TurnItem::ToolCall {
                        name,
                        status: ToolStatus::Completed,
                        ..
                    },
                    ..
                }) if name == "todo_write"
            )
        })
        .count();
    assert_eq!(settled, 2);
}

/// L4 (core seam): a write with every item completed pairs a durable
/// `NodeKind::Todos` commit — asserted on the tree facts, not the panel.
/// MUTATION CHECK: drop the Plan arm from `commit_item`'s node pairing.
/// Expected RUNTIME failure: no Todos node below.
#[tokio::test]
async fn all_completed_write_commits_a_todos_node() {
    let mut script = Vec::new();
    script.extend(todo_call(
        "plan-open",
        serde_json::json!([
            { "id": 0, "text": "fix the bug", "state": "processing" },
        ]),
    ));
    script.extend(todo_call(
        "plan-done",
        serde_json::json!([
            { "id": 0, "text": "fix the bug", "state": "completed" },
        ]),
    ));
    script.extend([FakeStep::Finish {
        reason: FinishReason::EndTurn,
    }]);
    let (handle, store) = actor(script);
    run_to_done(&handle, "fix the bug").await;
    let payloads = typed_payloads(&store).await;
    let todos_nodes: Vec<_> = payloads
        .iter()
        .filter_map(|(payload, _)| match payload {
            EventPayload::NodeCommitted(node) => match &node.kind {
                NodeKind::Todos { items } => Some(items.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(todos_nodes.len(), 1, "exactly one Todos node");
    assert_eq!(todos_nodes[0].len(), 1);
    assert_eq!(todos_nodes[0][0].state, TodoState::Completed);
    assert_eq!(todos_nodes[0][0].text, "fix the bug");
}

/// A FIRST write already all-completed still journals a full Started →
/// Completed lifecycle (the projection needs the pair to pin and unpin) and
/// commits the Todos node.
#[tokio::test]
async fn born_finished_plan_closes_its_lifecycle_immediately() {
    let mut script = Vec::new();
    script.extend(todo_call(
        "plan-instant",
        serde_json::json!([
            { "id": 0, "text": "already done", "state": "completed" },
        ]),
    ));
    script.extend([FakeStep::Finish {
        reason: FinishReason::EndTurn,
    }]);
    let (handle, store) = actor(script);
    run_to_done(&handle, "record the finished work").await;
    let payloads = typed_payloads(&store).await;
    let plans = plan_events(&payloads);
    assert_eq!(plans.len(), 2);
    assert!(matches!(plans[0].0, ItemEvent::Started { .. }));
    assert!(matches!(plans[1].0, ItemEvent::Completed { .. }));
    assert!(payloads.iter().any(|(payload, _)| matches!(
        payload,
        EventPayload::NodeCommitted(node) if matches!(node.kind, NodeKind::Todos { .. })
    )));
}

/// L2 at runtime: an invalid list (duplicate ids) settles as a typed
/// REJECTED tool result — no Plan facts, no turn failure; the model retries.
/// MUTATION CHECK: let validation errors escape as `DriveError`. Expected
/// RUNTIME failure: the turn errors instead of completing Done with the
/// rejection echo below.
#[tokio::test]
async fn invalid_list_is_rejected_without_plan_facts_or_turn_failure() {
    let mut script = Vec::new();
    script.extend(todo_call(
        "plan-bad",
        serde_json::json!([
            { "id": 1, "text": "first", "state": "listed" },
            { "id": 1, "text": "second", "state": "listed" },
        ]),
    ));
    script.extend([
        FakeStep::EmitText {
            text: "let me fix that list".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let (handle, store) = actor(script);
    run_to_done(&handle, "plan with a broken list").await;
    let payloads = typed_payloads(&store).await;
    assert!(plan_events(&payloads).is_empty(), "no Plan facts journal");
    let (_, result, _) = payloads
        .iter()
        .find_map(|(payload, render)| match payload {
            EventPayload::ToolResult { call_id, result } => Some((call_id, result, render)),
            _ => None,
        })
        .expect("rejection is a completed tool result");
    assert!(result.preview.contains("\"status\":\"rejected\""));
    assert!(result.preview.contains("invalid_argument"));
    assert!(result.preview.contains("duplicate todo id 1"));
}

/// The empty-list split pinned (brief decision 3): with NOTHING ever listed,
/// an empty write journals no Plan facts at all; after a real list, an empty
/// write emits Completed{Plan, []} (the projection unpins the panel) WITHOUT
/// a Todos node, and the next list starts a FRESH item id.
/// MUTATION CHECK: journal Started{Plan} for the nothing-listed empty write,
/// or reuse the cleared item id for the follow-up list. Expected RUNTIME
/// failure: a phantom empty panel pins, or the reborn plan reuses the closed
/// id (which the projection ignores forever).
#[tokio::test]
async fn empty_list_is_a_noop_when_nothing_listed_and_a_clear_after_a_list() {
    let mut script = Vec::new();
    script.extend(todo_call("plan-empty-noop", serde_json::json!([])));
    script.extend(todo_call(
        "plan-real",
        serde_json::json!([
            { "id": 0, "text": "step one", "state": "processing" },
        ]),
    ));
    script.extend(todo_call("plan-clear", serde_json::json!([])));
    script.extend(todo_call(
        "plan-reborn",
        serde_json::json!([
            { "id": 0, "text": "new direction", "state": "processing" },
        ]),
    ));
    script.extend([FakeStep::Finish {
        reason: FinishReason::EndTurn,
    }]);
    let (handle, store) = actor(script);
    run_to_done(&handle, "replan twice").await;
    let payloads = typed_payloads(&store).await;
    let plans = plan_events(&payloads);
    // no-op empty write journals nothing; then Started(real), Completed([]),
    // Started(reborn).
    assert_eq!(plans.len(), 3);
    let (
        ItemEvent::Started {
            item_id: real_id, ..
        },
        _,
    ) = plans[0]
    else {
        panic!("real list Starts a lifecycle, got {:?}", plans[0].0);
    };
    let (
        ItemEvent::Completed {
            item_id: clear_id,
            item: TurnItem::Plan { items },
        },
        _,
    ) = plans[1]
    else {
        panic!("clear Completes the open lifecycle, got {:?}", plans[1].0);
    };
    assert!(items.is_empty(), "the clear carries the empty list");
    assert_eq!(real_id, clear_id);
    let (
        ItemEvent::Started {
            item_id: reborn_id, ..
        },
        _,
    ) = plans[2]
    else {
        panic!("reborn list Starts a fresh lifecycle, got {:?}", plans[2].0);
    };
    assert_ne!(
        reborn_id, real_id,
        "a cleared lifecycle's item id is closed; the reborn plan needs a fresh one"
    );
    assert!(
        !payloads.iter().any(|(payload, _)| matches!(
            payload,
            EventPayload::NodeCommitted(node) if matches!(node.kind, NodeKind::Todos { .. })
        )),
        "a cleared plan is not a completed plan — no Todos node"
    );
    // Every echo answered ok — the no-op empty write included.
    let ok_echoes = payloads
        .iter()
        .filter(|(payload, _)| {
            matches!(
                payload,
                EventPayload::ToolResult { result, .. } if result.preview.contains("\"ok\":true")
            )
        })
        .count();
    assert_eq!(ok_echoes, 4);
}

/// Review pin (coordinator, G1 review of record): the plan lifecycle is
/// RUN-SCOPED. An unfinished plan from run 1 does not leak into run 2 — the
/// next run's first `todo_write` starts a FRESH lifecycle with its own
/// Started{Plan} fact and a different item id, so every run's plan facts are
/// self-contained (branch/checkpoint replay slices a run and must find the
/// Started).
/// MUTATION CHECK (executed): drop the `plan.run_id == *run_id` filter in
/// `emit_plan_facts`. Expected RUNTIME failure: run 2's first plan fact
/// becomes a Completed under run 1's item id — the second-Started assert and
/// the distinct-id assert below both fail.
#[tokio::test]
async fn an_unfinished_plan_does_not_leak_into_the_next_run() {
    let mut script = Vec::new();
    script.extend(todo_call(
        "plan-r1",
        serde_json::json!([
            { "id": 0, "text": "scope entrypoints", "state": "processing" },
            { "id": 1, "text": "patch run loop", "state": "listed", "dep": 0 },
        ]),
    ));
    script.extend([
        FakeStep::EmitText {
            text: "pausing here".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    script.extend(todo_call(
        "plan-r2",
        serde_json::json!([
            { "id": 0, "text": "scope entrypoints", "state": "completed" },
            { "id": 1, "text": "patch run loop", "state": "processing", "dep": 0 },
        ]),
    ));
    script.extend([
        FakeStep::EmitText {
            text: "resuming the plan".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let (handle, store) = actor(script);
    run_to_done(&handle, "start the refactor").await;
    run_to_done(&handle, "keep going").await;
    let payloads = typed_payloads(&store).await;
    let plans = plan_events(&payloads);
    assert_eq!(plans.len(), 2, "one Started per run, nothing else");
    let (
        ItemEvent::Started {
            item_id: first_run_id,
            ..
        },
        _,
    ) = plans[0]
    else {
        panic!("run 1's plan fact must be Started, got {:?}", plans[0].0);
    };
    let (
        ItemEvent::Started {
            item_id: second_run_id,
            item: TurnItem::Plan { items },
        },
        _,
    ) = plans[1]
    else {
        panic!(
            "run 2's first plan fact must be a fresh Started, got {:?}",
            plans[1].0
        );
    };
    assert_ne!(
        first_run_id, second_run_id,
        "a new run mints a fresh plan item id"
    );
    assert_eq!(items[0].state, TodoState::Completed);
    assert_eq!(items[1].state, TodoState::Processing);
}
