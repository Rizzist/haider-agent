#![allow(clippy::expect_used)]

use async_trait::async_trait;
use haider_core::{
    CancelToken, HarnessActor, HarnessConfig, MemoryStore, RetrySleeper, SubmitTurn,
    ToolDispatchResult, ToolDispatcher, retry_jittered_backoff_ms,
};
use haider_protocol::EventPayload;
use haider_protocol::error::HaiderError;
use haider_protocol::ids::{DeviceId, ItemId, RunId, SessionId};
use haider_protocol::item::{ItemEvent, ToolStatus, TurnItem};
use haider_protocol::provider::FinishReason;
use haider_protocol::state::RunState;
use haider_protocol::tool::{BoundedResult, ToolResultStatus};
use haider_provider::{FakeProvider, FakeStep, ProviderErrorKind};
use std::sync::{Arc, Mutex};

#[derive(Debug, Default)]
struct RecordingSleeper(Mutex<Vec<u64>>);

#[derive(Debug)]
struct StatusDispatcher {
    status: ToolResultStatus,
    reason: Option<String>,
}

#[async_trait]
impl ToolDispatcher for StatusDispatcher {
    async fn execute(
        &self,
        _run_id: &RunId,
        _item_id: &ItemId,
        _call_id: &str,
        _name: &str,
        _args: serde_json::Value,
        _cancel: &CancelToken,
    ) -> Result<ToolDispatchResult, HaiderError> {
        Ok(ToolDispatchResult::Completed(BoundedResult {
            preview: "tool result".into(),
            truncated: false,
            artifact: None,
            cursor: None,
            status: self.status,
            reason: self.reason.clone(),
        }))
    }
}

#[async_trait]
impl RetrySleeper for RecordingSleeper {
    async fn sleep(&self, delay_ms: u64) {
        self.0.lock().expect("sleeper lock").push(delay_ms);
    }
}

fn spawn(
    session: &str,
    script: Vec<FakeStep>,
    sleeper: Arc<RecordingSleeper>,
) -> (
    haider_core::HarnessHandle,
    Arc<MemoryStore>,
    Arc<FakeProvider>,
) {
    let mut config = HarnessConfig::for_session(SessionId::new(session), DeviceId::new("e1"), 1, 1);
    config.retry_sleeper = sleeper;
    let provider = Arc::new(FakeProvider::new(script));
    let store = Arc::new(MemoryStore::new());
    let handle = HarnessActor::spawn(config, provider.clone(), store.clone());
    (handle, store, provider)
}

/// LAW E1a: the actor copies the dispatcher terminal status instead of
/// unconditionally closing the tool as Completed. MUTATION: hard-code
/// `ToolStatus::Completed` at that join and the rejected assertion fails.
#[tokio::test]
async fn e1a_actor_preserves_failed_and_successful_tool_status() {
    for (suffix, result_status, expected) in [
        ("denied", ToolResultStatus::Rejected, ToolStatus::Rejected),
        ("ok", ToolResultStatus::Completed, ToolStatus::Completed),
    ] {
        let session = SessionId::new(format!("e1-tool-{suffix}"));
        let provider = Arc::new(FakeProvider::new(vec![
            FakeStep::EmitToolCall {
                call_id: format!("call-{suffix}"),
                name: "fs_write".into(),
                args: serde_json::json!({}),
            },
            FakeStep::Finish {
                reason: FinishReason::ToolUse,
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ]));
        let store = Arc::new(MemoryStore::new());
        let config = HarnessConfig::for_session(session.clone(), DeviceId::new("e1"), 1, 1);
        let (actor, handle) = HarnessActor::new_with_dispatcher(
            config,
            provider,
            store.clone(),
            Some(Arc::new(StatusDispatcher {
                status: result_status,
                reason: (result_status != ToolResultStatus::Completed)
                    .then(|| "effect denied by policy".into()),
            })),
        );
        let actor_task = tokio::spawn(actor.run());
        let outcome = handle
            .submit_turn(SubmitTurn::new("tool"))
            .await
            .expect("accepted")
            .wait()
            .await
            .expect("outcome");
        assert_eq!(outcome.state, RunState::Done);
        let completed = store
            .events(&session)
            .await
            .into_iter()
            .filter_map(|event| serde_json::from_value::<EventPayload>(event.payload).ok())
            .find_map(|payload| match payload {
                EventPayload::Item(ItemEvent::Completed {
                    item: TurnItem::ToolCall { status, .. },
                    ..
                }) => Some(status),
                _ => None,
            })
            .expect("completed tool row");
        assert_eq!(completed, expected);
        drop(handle);
        actor_task.await.expect("actor task");
    }
}

/// LAW E1b: refusal-only responses leave a durable, visible refusal row and
/// finish Done. MUTATION: deleting the RefusalDelta/surfacing arm removes the
/// row and fails this test at runtime.
#[tokio::test]
async fn e1b_refusal_only_is_visible_done_and_nonempty() {
    let sleeper = Arc::new(RecordingSleeper::default());
    let (handle, store, _) = spawn(
        "e1-refusal",
        vec![
            FakeStep::EmitRefusal {
                text: "  I cannot help with that.\n".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::Refusal,
            },
        ],
        sleeper,
    );
    let outcome = handle
        .submit_turn(SubmitTurn::new("refuse"))
        .await
        .expect("accepted")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Done);
    let events = store.events(&SessionId::new("e1-refusal")).await;
    let refusals = events
        .iter()
        .filter_map(|envelope| {
            serde_json::from_value::<EventPayload>(envelope.payload.clone()).ok()
        })
        .filter_map(|payload| match payload {
            EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::Refusal { reason },
                ..
            }) => Some(reason),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(refusals, vec!["I cannot help with that."]);
}

#[tokio::test]
async fn e1b_normal_completion_has_no_refusal_row() {
    let sleeper = Arc::new(RecordingSleeper::default());
    let (handle, store, _) = spawn(
        "e1-normal",
        vec![
            FakeStep::EmitText { text: "ok".into() },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ],
        sleeper,
    );
    let outcome = handle
        .submit_turn(SubmitTurn::new("answer"))
        .await
        .expect("accepted")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Done);
    assert!(
        store
            .events(&SessionId::new("e1-normal"))
            .await
            .iter()
            .filter_map(|event| serde_json::from_value::<EventPayload>(event.payload.clone()).ok())
            .all(|payload| !matches!(
                payload,
                EventPayload::Item(ItemEvent::Completed {
                    item: TurnItem::Refusal { .. },
                    ..
                })
            ))
    );
}

/// LAW E1e: a clean pre-content EOF is a retryable interruption and recovery
/// on the next attempt completes. MUTATION: reclassifying EOF as malformed
/// makes this terminal and fails the attempt/wait assertions.
#[tokio::test]
async fn e1e_premature_eof_before_content_retries_and_recovers() {
    let sleeper = Arc::new(RecordingSleeper::default());
    let (handle, store, provider) = spawn(
        "e1-eof",
        vec![
            FakeStep::PrematureEof,
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ],
        sleeper.clone(),
    );
    let outcome = handle
        .submit_turn(SubmitTurn::new("recover"))
        .await
        .expect("accepted")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Done);
    assert_eq!(provider.requests().len(), 2);
    let run_id = store
        .events(&SessionId::new("e1-eof"))
        .await
        .into_iter()
        .find_map(|event| event.run_id)
        .expect("run id");
    assert_eq!(
        *sleeper.0.lock().expect("sleeper lock"),
        vec![retry_jittered_backoff_ms(&run_id, 1)]
    );
}

/// LAW E1c: quota never spends retry budget even if a broken classifier marks
/// it retryable. MUTATION: removing the kind gate yields another attempt.
#[tokio::test]
async fn e1c_quota_exhaustion_terminalizes_without_retry_budget() {
    let sleeper = Arc::new(RecordingSleeper::default());
    let (handle, _, provider) = spawn(
        "e1-quota",
        vec![FakeStep::ErrorWithRetryability {
            kind: ProviderErrorKind::QuotaExhausted,
            message: "provider quota/credit exhausted — retrying will not help".into(),
            retryable: true,
            retry_after_ms: None,
        }],
        sleeper.clone(),
    );
    let outcome = handle
        .submit_turn(SubmitTurn::new("quota"))
        .await
        .expect("accepted")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Errored);
    assert_eq!(provider.requests().len(), 1);
    assert!(sleeper.0.lock().expect("sleeper lock").is_empty());
}

#[tokio::test]
async fn e1e_permanent_connection_configuration_does_not_retry() {
    let sleeper = Arc::new(RecordingSleeper::default());
    let (handle, _, provider) = spawn(
        "e1-permanent-connection",
        vec![FakeStep::ErrorWithRetryability {
            kind: ProviderErrorKind::ConnectionConfiguration,
            message: "certificate trust failure; check endpoint/proxy configuration".into(),
            retryable: true,
            retry_after_ms: None,
        }],
        sleeper.clone(),
    );
    let outcome = handle
        .submit_turn(SubmitTurn::new("connect"))
        .await
        .expect("accepted")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Errored);
    assert_eq!(provider.requests().len(), 1);
    assert!(sleeper.0.lock().expect("sleeper lock").is_empty());
}
