#![allow(clippy::expect_used)]

use async_trait::async_trait;
use haider_core::{
    CommittedRange, HarnessActor, HarnessConfig, HarnessHandle, MemoryStore, StoreHandle,
    SubmitTurn,
};
use haider_protocol::EventPayload;
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::ids::{DeviceId, ItemId, SessionId};
use haider_protocol::item::{ItemEvent, ToolStatus, TurnItem};
use haider_protocol::provider::{CapabilityDoc, FinishReason, Usage, UsageSource};
use haider_protocol::state::RunState;
use haider_provider::{
    FakeProvider, FakeStep, Provider, ProviderError, ProviderStream, TurnRequest,
};
use std::collections::HashSet;
use std::future::pending;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;
use tokio::time::{Duration, sleep, timeout};

const SESSION: &str = "session-test";

fn config() -> HarnessConfig {
    HarnessConfig::for_session(
        SessionId::new(SESSION),
        DeviceId::new("device-test"),
        17,
        23,
    )
}

fn runtime(script: Vec<FakeStep>) -> (HarnessHandle, Arc<MemoryStore>, Arc<FakeProvider>) {
    let provider = Arc::new(FakeProvider::new(script));
    let store = Arc::new(MemoryStore::new());
    let handle = HarnessActor::spawn(config(), provider.clone(), store.clone());
    (handle, store, provider)
}

fn typed(envelope: &RawEnvelope) -> EventPayload {
    serde_json::from_value(envelope.payload.clone()).expect("known payload")
}

fn normalize(mut payload: serde_json::Value) -> serde_json::Value {
    if payload["type"] == "item" {
        payload["item_id"] = serde_json::Value::String("<item>".into());
    }
    payload
}

fn assert_items_closed_before_terminal(events: &[RawEnvelope]) {
    let mut open = HashSet::<ItemId>::new();
    let mut saw_terminal = false;
    for event in events {
        assert!(!saw_terminal, "an envelope followed the terminal run state");
        match typed(event) {
            EventPayload::Item(ItemEvent::Started { item_id, .. }) => {
                assert!(open.insert(item_id), "an item started twice");
            }
            EventPayload::Item(ItemEvent::Completed { item_id, .. }) => {
                assert!(open.remove(&item_id), "completed item was not open");
            }
            EventPayload::RunState(state) if state.is_terminal() => {
                assert!(open.is_empty(), "terminal run state left open items");
                saw_terminal = true;
            }
            _ => {}
        }
    }
    assert!(saw_terminal, "fixture did not commit a terminal run state");
    assert!(open.is_empty(), "fixture ended with open items");
}

#[tokio::test]
async fn full_turn_commits_exact_projected_sequence() {
    let usage = Usage {
        input: 11,
        output: 7,
        reasoning: 2,
        cached: 3,
        source: UsageSource::ProviderReported,
        account: None,
    };
    let (handle, store, provider) = runtime(vec![
        FakeStep::EmitText {
            text: "hello".into(),
        },
        FakeStep::EmitToolCall {
            call_id: "call-1".into(),
            name: "inspect".into(),
            args: serde_json::json!({"path":"src/lib.rs"}),
        },
        FakeStep::EmitUsage {
            usage: usage.clone(),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
    ]);
    let turn = handle
        .submit_turn(SubmitTurn::new("do the work"))
        .await
        .expect("turn accepted");
    let outcome = turn.wait().await.expect("actor reports outcome");
    assert_eq!(outcome.state, RunState::Done);
    assert_eq!(outcome.finish_reason, FinishReason::ToolUse);

    let events = store.events(&SessionId::new(SESSION)).await;
    let actual: Vec<_> = events
        .iter()
        .map(|event| normalize(event.payload.clone()))
        .collect();
    let expected = vec![
        serde_json::json!({"type":"run_state","state":"queued"}),
        serde_json::json!({
            "type":"user_message",
            "text":"do the work",
            "attachments":[],
            "mode":"steer"
        }),
        serde_json::json!({"type":"run_state","state":"thinking"}),
        serde_json::json!({"type":"run_state","state":"streaming"}),
        serde_json::json!({
            "type":"item",
            "event":"started",
            "item_id":"<item>",
            "item":{"item":"agent_message","text":""}
        }),
        serde_json::json!({
            "type":"item",
            "event":"delta",
            "item_id":"<item>",
            "delta":{"delta":"text","text":"hello"}
        }),
        serde_json::json!({
            "type":"item",
            "event":"completed",
            "item_id":"<item>",
            "item":{"item":"agent_message","text":"hello"}
        }),
        serde_json::json!({
            "type":"item",
            "event":"started",
            "item_id":"<item>",
            "item":{
                "item":"tool_call",
                "call_id":"call-1",
                "name":"inspect",
                "args":{},
                "status":"in_progress"
            }
        }),
        serde_json::json!({
            "type":"item",
            "event":"delta",
            "item_id":"<item>",
            "delta":{
                "delta":"tool_args",
                "fragment":"{\"path\":\"src/lib.rs\"}"
            }
        }),
        serde_json::json!({
            "type":"item",
            "event":"completed",
            "item_id":"<item>",
            "item":{
                "item":"tool_call",
                "call_id":"call-1",
                "name":"inspect",
                "args":{"path":"src/lib.rs"},
                "status":"pending"
            }
        }),
        serde_json::to_value(EventPayload::Usage(usage)).expect("usage serializes"),
        serde_json::json!({"type":"run_state","state":"done"}),
    ];
    assert_eq!(actual, expected);
    assert!(events.windows(2).all(|pair| pair[1].seq == pair[0].seq + 1));
    assert!(
        events
            .iter()
            .all(|event| event.authority_epoch == 17 && event.worker_generation == 23)
    );

    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].model, "fake-model");
    assert_eq!(requests[0].max_tokens, 4096);
}

#[tokio::test]
async fn split_utf8_reassembles_in_completed_agent_message() {
    let (handle, store, _) = runtime(vec![
        FakeStep::SplitUtf8 {
            text: "A🌍B".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let outcome = handle
        .submit_turn(SubmitTurn::new("utf8"))
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Done);

    let events = store.events(&SessionId::new(SESSION)).await;
    let completed = events.iter().find_map(|event| match typed(event) {
        EventPayload::Item(ItemEvent::Completed {
            item: TurnItem::AgentMessage { text },
            ..
        }) => Some(text),
        _ => None,
    });
    assert_eq!(completed.as_deref(), Some("A🌍B"));
}

#[tokio::test]
async fn malformed_frame_is_errored_with_typed_error() {
    let (handle, store, _) = runtime(vec![FakeStep::MalformedFrame]);
    let outcome = handle
        .submit_turn(SubmitTurn::new("break safely"))
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Errored);
    let error = outcome.error.expect("typed error");
    assert_eq!(error.code, ErrorCode::ProviderError);
    assert_eq!(
        error.details.expect("typed details")["provider_error_kind"],
        "MalformedFrame"
    );

    let events = store.events(&SessionId::new(SESSION)).await;
    assert!(matches!(
        events.last().map(typed),
        Some(EventPayload::RunState(RunState::Errored))
    ));
}

#[tokio::test]
async fn provider_error_mid_tool_completes_failed_item_before_errored() {
    let (handle, store, _) = runtime(vec![
        FakeStep::EmitToolCallStart {
            call_id: "call-open".into(),
            name: "inspect".into(),
        },
        FakeStep::MalformedFrame,
    ]);
    let outcome = handle
        .submit_turn(SubmitTurn::new("open then fail"))
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Errored);

    let events = store.events(&SessionId::new(SESSION)).await;
    assert_items_closed_before_terminal(&events);
    assert!(events.iter().any(|event| {
        matches!(
            typed(event),
            EventPayload::Item(ItemEvent::Completed {
                item:
                    TurnItem::ToolCall {
                        ref call_id,
                        status: ToolStatus::Failed,
                        ..
                    },
                ..
            }) if call_id == "call-open"
        )
    }));
    assert!(matches!(
        events.last().map(typed),
        Some(EventPayload::RunState(RunState::Errored))
    ));
}

#[tokio::test]
async fn cancellation_mid_tool_completes_tool_as_cancelled_never_failed() {
    // Frozen law: cancellation is an outcome, never a failure — an open tool at
    // cancel time closes with ToolStatus::Cancelled (review r2 regression).
    let (handle, store, _) = runtime(vec![
        FakeStep::EmitToolCallStart {
            call_id: "c-1".into(),
            name: "fs_read".into(),
        },
        FakeStep::EmitToolArgsDelta {
            call_id: "c-1".into(),
            fragment: "{\"pa".into(),
        },
        FakeStep::Delay { ms: 300 },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let mut subscriber = handle.subscribe();
    let turn = handle
        .submit_turn(SubmitTurn::new("cancel mid tool"))
        .await
        .expect("turn accepted");
    timeout(Duration::from_secs(1), async {
        loop {
            let event = subscriber.recv().await.expect("event");
            if matches!(
                typed(&event),
                EventPayload::Item(ItemEvent::Started {
                    item: TurnItem::ToolCall { .. },
                    ..
                })
            ) {
                break;
            }
        }
    })
    .await
    .expect("tool started");
    turn.cancel();
    let outcome = turn.wait().await.expect("outcome");
    assert_eq!(outcome.state, RunState::Cancelled);
    let events = store.events(&SessionId::new(SESSION)).await;
    assert_items_closed_before_terminal(&events);
    let tool_terminal = events.iter().find_map(|event| match typed(event) {
        EventPayload::Item(ItemEvent::Completed {
            item: TurnItem::ToolCall { status, .. },
            ..
        }) => Some(status),
        _ => None,
    });
    assert_eq!(
        tool_terminal,
        Some(haider_protocol::item::ToolStatus::Cancelled),
        "open tool must close as Cancelled, never Failed"
    );
}

#[tokio::test]
async fn cancellation_mid_stream_is_terminal_and_discards_later_events() {
    let (handle, store, _) = runtime(vec![
        FakeStep::EmitText {
            text: "first".into(),
        },
        FakeStep::Delay { ms: 200 },
        FakeStep::EmitText {
            text: "second".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let mut subscriber = handle.subscribe();
    let turn = handle
        .submit_turn(SubmitTurn::new("cancel me"))
        .await
        .expect("turn accepted");

    timeout(Duration::from_secs(1), async {
        loop {
            let event = subscriber.recv().await.expect("event");
            if matches!(typed(&event), EventPayload::Item(ItemEvent::Delta { .. })) {
                break;
            }
        }
    })
    .await
    .expect("first delta arrives");
    turn.cancel();
    let outcome = turn.wait().await.expect("outcome");
    assert_eq!(outcome.state, RunState::Cancelled);
    assert_eq!(outcome.finish_reason, FinishReason::Cancelled);
    assert!(outcome.error.is_none());

    let before = store.events(&SessionId::new(SESSION)).await;
    assert_items_closed_before_terminal(&before);
    assert!(before.iter().any(|event| {
        matches!(
            typed(event),
            EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::AgentMessage { ref text },
                ..
            }) if text == "first"
        )
    }));
    assert!(matches!(
        before.last().map(typed),
        Some(EventPayload::RunState(RunState::Cancelled))
    ));
    assert!(!before.iter().any(|event| {
        matches!(
            typed(event),
            EventPayload::Item(ItemEvent::Delta {
                delta: haider_protocol::item::ItemDelta::Text { ref text },
                ..
            }) if text == "second"
        )
    }));
    sleep(Duration::from_millis(250)).await;
    let after = store.events(&SessionId::new(SESSION)).await;
    assert_eq!(after.len(), before.len(), "nothing follows Cancelled");
}

#[tokio::test]
async fn cancellation_during_finish_item_close_wins_before_done() {
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitText {
            text: "selected finish".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let store = Arc::new(BlockingCompletedStore::new());
    let handle = HarnessActor::spawn(config(), provider, store.clone());
    let turn = handle
        .submit_turn(SubmitTurn::new("cancel finish"))
        .await
        .expect("turn accepted");

    timeout(Duration::from_secs(1), store.blocked.notified())
        .await
        .expect("item completion reached store");
    turn.cancel();
    store.release.notify_one();
    let outcome = timeout(Duration::from_secs(1), turn.wait())
        .await
        .expect("turn terminates")
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Cancelled);

    let events = store.inner.events(&SessionId::new(SESSION)).await;
    assert_items_closed_before_terminal(&events);
    assert!(matches!(
        events.last().map(typed),
        Some(EventPayload::RunState(RunState::Cancelled))
    ));
}

#[tokio::test]
async fn cancellation_interrupts_provider_stream_startup() {
    let provider = Arc::new(HangingStartProvider {
        entered: Notify::new(),
    });
    let store = Arc::new(MemoryStore::new());
    let handle = HarnessActor::spawn(config(), provider.clone(), store.clone());
    let turn = handle
        .submit_turn(SubmitTurn::new("cancel startup"))
        .await
        .expect("turn accepted");

    timeout(Duration::from_secs(1), provider.entered.notified())
        .await
        .expect("provider startup entered");
    turn.cancel();
    let outcome = timeout(Duration::from_secs(1), turn.wait())
        .await
        .expect("startup cancellation terminates")
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Cancelled);
    assert!(matches!(
        store
            .events(&SessionId::new(SESSION))
            .await
            .last()
            .map(typed),
        Some(EventPayload::RunState(RunState::Cancelled))
    ));
}

#[tokio::test]
async fn hanging_provider_can_always_be_cancelled() {
    let (handle, store, _) = runtime(vec![FakeStep::Hang]);
    let mut subscriber = handle.subscribe();
    let turn = handle
        .submit_turn(SubmitTurn::new("hang"))
        .await
        .expect("turn accepted");
    timeout(Duration::from_secs(1), async {
        loop {
            let event = subscriber.recv().await.expect("event");
            if matches!(typed(&event), EventPayload::RunState(RunState::Streaming)) {
                break;
            }
        }
    })
    .await
    .expect("streaming state arrives");

    turn.cancel();
    let outcome = timeout(Duration::from_secs(1), turn.wait())
        .await
        .expect("cancel completes")
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Cancelled);
    assert!(matches!(
        store
            .events(&SessionId::new(SESSION))
            .await
            .last()
            .map(typed),
        Some(EventPayload::RunState(RunState::Cancelled))
    ));
}

#[tokio::test]
async fn actor_restarts_do_not_reuse_run_or_item_ids() {
    let store = Arc::new(MemoryStore::new());
    let script = || {
        vec![
            FakeStep::EmitText { text: "id".into() },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ]
    };
    let first = HarnessActor::spawn(
        config(),
        Arc::new(FakeProvider::new(script())),
        store.clone(),
    );
    let mut restarted_config = config();
    restarted_config.worker_generation += 1;
    let second = HarnessActor::spawn(
        restarted_config,
        Arc::new(FakeProvider::new(script())),
        store.clone(),
    );

    first
        .submit_turn(SubmitTurn::new("first actor"))
        .await
        .expect("first accepted")
        .wait()
        .await
        .expect("first outcome");
    second
        .submit_turn(SubmitTurn::new("second actor"))
        .await
        .expect("second accepted")
        .wait()
        .await
        .expect("second outcome");

    let events = store.events(&SessionId::new(SESSION)).await;
    let run_ids: HashSet<_> = events
        .iter()
        .filter_map(|event| event.run_id.as_ref().map(ToString::to_string))
        .collect();
    let item_ids: HashSet<_> = events
        .iter()
        .filter_map(|event| match typed(event) {
            EventPayload::Item(ItemEvent::Started { item_id, .. }) => Some(item_id.to_string()),
            _ => None,
        })
        .collect();

    assert_eq!(run_ids.len(), 2);
    assert!(
        run_ids
            .iter()
            .any(|id| id.starts_with("run-session-test-23-"))
    );
    assert!(
        run_ids
            .iter()
            .any(|id| id.starts_with("run-session-test-24-"))
    );
    assert_eq!(item_ids.len(), 2);
    assert!(
        item_ids
            .iter()
            .any(|id| id.starts_with("item-session-test-23-"))
    );
    assert!(
        item_ids
            .iter()
            .any(|id| id.starts_with("item-session-test-24-"))
    );
}

#[tokio::test]
async fn memory_store_allocates_and_reads_committed_sequences() {
    let (handle, store, _) = runtime(vec![FakeStep::Finish {
        reason: FinishReason::EndTurn,
    }]);
    handle
        .submit_turn(SubmitTurn::new("store"))
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("outcome");

    assert_eq!(
        StoreHandle::latest_seq(store.as_ref(), &SessionId::new(SESSION))
            .await
            .expect("latest"),
        5
    );
    let tail = StoreHandle::read(store.as_ref(), &SessionId::new(SESSION), 3, 10)
        .await
        .expect("read");
    assert_eq!(
        tail.iter().map(|event| event.seq).collect::<Vec<_>>(),
        vec![4, 5]
    );
}

#[test]
fn completed_tool_call_is_pending_execution() {
    let item = TurnItem::ToolCall {
        call_id: "call".into(),
        name: "tool".into(),
        args: serde_json::json!({}),
        status: ToolStatus::Pending,
    };
    assert!(matches!(
        item,
        TurnItem::ToolCall {
            status: ToolStatus::Pending,
            ..
        }
    ));
}

struct BlockingCompletedStore {
    inner: MemoryStore,
    did_block: AtomicBool,
    blocked: Notify,
    release: Notify,
}

impl BlockingCompletedStore {
    fn new() -> Self {
        Self {
            inner: MemoryStore::new(),
            did_block: AtomicBool::new(false),
            blocked: Notify::new(),
            release: Notify::new(),
        }
    }
}

#[async_trait]
impl StoreHandle for BlockingCompletedStore {
    async fn append(&self, envelopes: &mut [RawEnvelope]) -> Result<CommittedRange, HaiderError> {
        let should_block = envelopes.first().is_some_and(|envelope| {
            envelope.payload["type"] == "item" && envelope.payload["event"] == "completed"
        });
        if should_block && !self.did_block.swap(true, Ordering::SeqCst) {
            self.blocked.notify_one();
            self.release.notified().await;
        }
        self.inner.append(envelopes).await
    }

    async fn read(
        &self,
        session_id: &SessionId,
        since_seq: u64,
        limit: usize,
    ) -> Result<Vec<RawEnvelope>, HaiderError> {
        self.inner.read(session_id, since_seq, limit).await
    }

    async fn latest_seq(&self, session_id: &SessionId) -> Result<u64, HaiderError> {
        self.inner.latest_seq(session_id).await
    }
}

struct HangingStartProvider {
    entered: Notify,
}

#[async_trait]
impl Provider for HangingStartProvider {
    async fn capabilities(&self) -> CapabilityDoc {
        FakeProvider::new(Vec::new()).capabilities().await
    }

    async fn stream_turn(&self, _request: TurnRequest) -> Result<ProviderStream, ProviderError> {
        self.entered.notify_one();
        pending().await
    }
}
