//! M2: model-authored failure text is data; only an authorized typed call
//! selects a task failure. The accepted call and terminal are crash-atomic.
#![allow(clippy::expect_used)]

use async_trait::async_trait;
use haider_core::{
    CommittedRange, HarnessActor, HarnessConfig, MemoryStore, StoreHandle, SubmitTurn,
};
use haider_protocol::EventPayload;
use haider_protocol::branch::BranchDescriptor;
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::ids::{BranchId, DeviceId, SessionId};
use haider_protocol::item::{ItemEvent, ToolStatus, TurnItem};
use haider_protocol::provider::FinishReason;
use haider_protocol::state::RunState;
use haider_protocol::task_outcome::TaskOutcomeV1;
use haider_provider::{FakeProvider, FakeStep, ToolDefinition};
use std::sync::Arc;

const SESSION: &str = "task-outcome-session";

fn config(advertised: bool) -> HarnessConfig {
    let mut config = HarnessConfig::for_session(
        SessionId::new(SESSION),
        DeviceId::new("task-outcome-device"),
        1,
        1,
    );
    config.enforce_advertised_tool_ceiling = true;
    if advertised {
        let manifest = haider_tools::task_outcome_manifest();
        config.tools.push(ToolDefinition {
            name: manifest.name,
            description: manifest.description,
            input_schema: manifest.input_schema,
        });
    }
    config
}

fn signal(args: serde_json::Value) -> FakeStep {
    FakeStep::EmitToolCall {
        call_id: "outcome-1".into(),
        name: "task_outcome".into(),
        args,
    }
}

fn failure() -> FakeStep {
    signal(serde_json::json!({"status": "failure", "reason": "Required input is unavailable"}))
}

fn payloads(events: &[RawEnvelope]) -> Vec<EventPayload> {
    events
        .iter()
        .map(|event| event.payload.decode_event().expect("typed event"))
        .collect()
}

#[tokio::test]
async fn free_text_failure_remains_done_without_a_task_outcome() {
    let store = Arc::new(MemoryStore::new());
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitText {
            text: r#"{"status":"FAILURE","category":"scripted"}"#.into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let handle = HarnessActor::spawn(config(true), provider.clone(), store.clone());
    let outcome = handle
        .submit_turn(SubmitTurn::new("report result"))
        .await
        .expect("accepted")
        .wait()
        .await
        .expect("completed");
    assert_eq!(outcome.state, RunState::Done);
    assert!(outcome.error.is_none());
    let events = store.events(&SessionId::new(SESSION)).await;
    assert!(
        events
            .iter()
            .all(|event| event.payload.get("task_outcome").is_none())
    );
    assert!(matches!(
        payloads(&events).last(),
        Some(EventPayload::RunState(RunState::Done))
    ));
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn typed_failure_acceptance_and_terminal_share_one_batch_and_stop_the_stream() {
    let store = Arc::new(MemoryStore::new());
    let provider = Arc::new(FakeProvider::new(vec![
        failure(),
        FakeStep::EmitText {
            text: "must not be consumed".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::EmitText {
            text: "must not request again".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let handle = HarnessActor::spawn(config(true), provider.clone(), store.clone());
    let mut batches = handle.subscribe_committed_batches();
    let outcome = handle
        .submit_turn(SubmitTurn::new("report result"))
        .await
        .expect("accepted")
        .wait()
        .await
        .expect("completed");
    assert_eq!(outcome.state, RunState::Errored);
    assert_eq!(outcome.error.expect("failure").code, ErrorCode::TaskFailed);
    assert_eq!(provider.requests().len(), 1);
    let events = store.events(&SessionId::new(SESSION)).await;
    let terminal = events.last().expect("terminal");
    assert_eq!(terminal.payload["task_outcome_version"], 1);
    assert_eq!(
        serde_json::from_value::<TaskOutcomeV1>(terminal.payload["task_outcome"].clone())
            .expect("typed task outcome"),
        TaskOutcomeV1::Failure {
            reason: "Required input is unavailable".into()
        }
    );
    let typed = payloads(&events);
    assert_eq!(
        typed
            .iter()
            .filter(|event| matches!(event, EventPayload::RunState(state) if state.is_terminal()))
            .count(),
        1
    );
    assert!(matches!(
        &typed[typed.len() - 2],
        EventPayload::RunFailed {
            code: ErrorCode::TaskFailed,
            retryable: false,
            ..
        }
    ));
    assert!(!format!("{typed:?}").contains("must not"));
    let mut terminal_batches = Vec::new();
    while let Ok(batch) = batches.try_recv() {
        if batch
            .iter()
            .any(|event| event.payload.get("task_outcome").is_some())
        {
            terminal_batches.push(batch);
        }
    }
    assert_eq!(terminal_batches.len(), 1);
    let batch = payloads(&terminal_batches[0]);
    assert!(matches!(
        batch.as_slice(),
        [
            EventPayload::ToolResult { .. },
            EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::ToolCall {
                    status: ToolStatus::Completed,
                    ..
                },
                ..
            }),
            EventPayload::NodeCommitted(_),
            EventPayload::RunFailed {
                code: ErrorCode::TaskFailed,
                ..
            },
            EventPayload::RunState(RunState::Errored),
        ]
    ));
}

#[tokio::test]
async fn invalid_or_unadvertised_signals_are_rejected_without_selecting_a_terminal() {
    for (advertised, args) in [
        (
            false,
            serde_json::json!({"status":"failure", "reason":"not authorized"}),
        ),
        (
            true,
            serde_json::json!({"status":"success", "reason":"unsupported"}),
        ),
        (
            true,
            serde_json::json!({"status":"FAILURE", "reason":"wrong enum"}),
        ),
        (true, serde_json::json!({"status":"failure", "reason":""})),
        (
            true,
            serde_json::json!({"status":"failure", "reason":"no", "unexpected":true}),
        ),
    ] {
        let store = Arc::new(MemoryStore::new());
        let provider = Arc::new(FakeProvider::new(vec![
            signal(args),
            FakeStep::Finish {
                reason: FinishReason::ToolUse,
            },
            FakeStep::ExpectToolResult {
                call_id: "outcome-1".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ]));
        let handle = HarnessActor::spawn(config(advertised), provider, store.clone());
        let outcome = handle
            .submit_turn(SubmitTurn::new("report result"))
            .await
            .expect("accepted")
            .wait()
            .await
            .expect("completed");
        assert_eq!(outcome.state, RunState::Done);
        let events = store.events(&SessionId::new(SESSION)).await;
        assert!(
            events
                .iter()
                .all(|event| event.payload.get("task_outcome").is_none())
        );
        assert!(payloads(&events).iter().any(|event| matches!(
            event,
            EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::ToolCall {
                    status: ToolStatus::Rejected,
                    ..
                },
                ..
            })
        )));
    }
}

struct RejectTaskTerminal(MemoryStore);

#[async_trait]
impl StoreHandle for RejectTaskTerminal {
    async fn append(&self, envelopes: &mut [RawEnvelope]) -> Result<CommittedRange, HaiderError> {
        if envelopes
            .iter()
            .any(|event| event.payload.get("task_outcome").is_some())
        {
            return Err(HaiderError::new(
                ErrorCode::StoreUnavailable,
                "injected task terminal append failure",
                false,
            ));
        }
        self.0.append(envelopes).await
    }

    async fn read(
        &self,
        session: &SessionId,
        since: u64,
        limit: usize,
    ) -> Result<Vec<RawEnvelope>, HaiderError> {
        self.0.read(session, since, limit).await
    }

    async fn latest_seq(&self, session: &SessionId) -> Result<u64, HaiderError> {
        self.0.latest_seq(session).await
    }

    async fn branch_lineage(
        &self,
        session: &SessionId,
        branch: Option<&BranchId>,
    ) -> Result<Vec<BranchDescriptor>, HaiderError> {
        self.0.branch_lineage(session, branch).await
    }
}

#[tokio::test]
async fn failed_terminal_append_never_leaves_an_accepted_signal_or_done() {
    let store = Arc::new(RejectTaskTerminal(MemoryStore::new()));
    let provider = Arc::new(FakeProvider::new(vec![
        failure(),
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let handle = HarnessActor::spawn(config(true), provider, store.clone());
    let outcome = handle
        .submit_turn(SubmitTurn::new("report result"))
        .await
        .expect("accepted")
        .wait()
        .await
        .expect("completed");
    assert_eq!(outcome.state, RunState::Errored);
    assert_eq!(
        outcome.error.expect("failure").code,
        ErrorCode::StoreUnavailable
    );
    let events = store.0.events(&SessionId::new(SESSION)).await;
    assert!(
        events
            .iter()
            .all(|event| event.payload.get("task_outcome").is_none())
    );
    assert!(payloads(&events).iter().all(|event| !matches!(
        event,
        EventPayload::ToolResult { .. } | EventPayload::RunState(RunState::Done)
    )));
}

#[tokio::test]
async fn task_outcome_rejects_a_signal_while_another_call_is_open() {
    let store = Arc::new(MemoryStore::new());
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCallStart {
            call_id: "other".into(),
            name: "fs_read".into(),
        },
        FakeStep::EmitToolArgsDelta {
            call_id: "other".into(),
            fragment: "{}".into(),
        },
        failure(),
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "outcome-1".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let handle = HarnessActor::spawn(config(true), provider, store.clone());
    let outcome = handle
        .submit_turn(SubmitTurn::new("report result"))
        .await
        .expect("accepted")
        .wait()
        .await
        .expect("completed");
    assert_eq!(outcome.state, RunState::Done);
    let events = store.events(&SessionId::new(SESSION)).await;
    assert!(
        events
            .iter()
            .all(|event| event.payload.get("task_outcome").is_none())
    );
    assert!(payloads(&events).iter().any(|event| matches!(event,
        EventPayload::ToolResult { call_id, result } if call_id == "outcome-1" && result.reason.as_ref().is_some_and(|reason| reason.contains("no pending tools")))));
}
