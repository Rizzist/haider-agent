#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use haider_core::{
    CancelToken, HarnessActor, HarnessConfig, MemoryStore, SubmitTurn, ToolDispatchResult,
    ToolDispatcher,
};
use haider_protocol::error::HaiderError;
use haider_protocol::ids::{DeviceId, ItemId, RunId, SessionId};
use haider_protocol::provider::{Block, FinishReason};
use haider_protocol::tool::{BoundedResult, ToolResultStatus};
use haider_provider::{FakeProvider, FakeStep};

async fn one_request(brief: Option<&str>) -> (haider_provider::TurnRequest, Arc<MemoryStore>) {
    let mut config = HarnessConfig::for_session(
        SessionId::new("graph-brief-cache-law"),
        DeviceId::new("graph-brief-test"),
        1,
        1,
    );
    config.system_prompt = Some("stable system".into());
    config.volatile_user_tail = brief.map(ToOwned::to_owned);
    let provider = Arc::new(FakeProvider::new(vec![FakeStep::Finish {
        reason: FinishReason::EndTurn,
    }]));
    let store = Arc::new(MemoryStore::new());
    let handle = HarnessActor::spawn(config, provider.clone(), store.clone());
    handle
        .submit_turn(SubmitTurn::new("durable user request"))
        .await
        .expect("submit")
        .wait()
        .await
        .expect("complete");
    (
        provider.requests().into_iter().next().expect("request"),
        store,
    )
}

/// CG-M1 LAW: GraphBrief is provider-visible immediately before the accepted
/// current user. It stays outside durable history while becoming an immutable
/// provider-prefix block for this turn, with its own exact-view epoch.
#[tokio::test]
async fn graph_brief_is_volatile_but_stable_inside_its_turn_epoch() {
    let brief = "GraphBrief: VERIFY attempt 2/8; gate all-of-3; evidence 1 green/0 red (1 effective); next: record 3 green VERIFY results.";
    let (baseline, _) = one_request(None).await;
    let (active, store) = one_request(Some(brief)).await;

    assert_eq!(active.messages.len(), baseline.messages.len() + 1);
    assert_eq!(
        &active.messages[1..],
        baseline.messages,
        "the frozen snapshot precedes the accepted current user"
    );
    assert!(active.messages.first().is_some_and(|message| {
        matches!(message.blocks.as_slice(), [Block::Text { text }] if text == brief)
    }));
    let active_metadata = active.cache_metadata.clone().expect("active metadata");
    let baseline_metadata = baseline.cache_metadata.expect("baseline metadata");
    assert_eq!(
        active_metadata.current_user_start,
        baseline_metadata.current_user_start + 1,
        "the request-local current-user boundary accounts for the inserted snapshot"
    );
    assert_eq!(
        active_metadata.stable_history_end,
        baseline_metadata.stable_history_end + 1,
        "the frozen snapshot is part of this turn's exact provider prefix"
    );
    assert_eq!(
        active_metadata.prefix_digests.system,
        baseline_metadata.prefix_digests.system
    );
    assert_eq!(
        active_metadata.prefix_digests.tools,
        baseline_metadata.prefix_digests.tools
    );
    assert_ne!(
        active_metadata.prefix_digests.immutable_history,
        baseline_metadata.prefix_digests.immutable_history
    );
    assert_ne!(active_metadata.cache_epoch, baseline_metadata.cache_epoch);
    assert!(
        active_metadata.stable_prefix_tokens > baseline_metadata.stable_prefix_tokens,
        "stable-prefix accounting includes the request-local snapshot bytes"
    );

    let durable = store.events(&SessionId::new("graph-brief-cache-law")).await;
    assert!(
        durable
            .iter()
            .all(|envelope| !envelope.payload.to_string().contains("GraphBrief:")),
        "GraphBrief never enters the durable prompt history"
    );
}

struct CountingSnapshotDispatcher {
    refreshes: AtomicUsize,
}

impl CountingSnapshotDispatcher {
    fn refresh_count(&self) -> usize {
        self.refreshes.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ToolDispatcher for CountingSnapshotDispatcher {
    async fn refresh_volatile_context_tail(&self) -> Result<Option<String>, HaiderError> {
        let ordinal = self.refreshes.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(Some(format!("GraphBrief snapshot {ordinal}")))
    }

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
            preview: "done".into(),
            truncated: false,
            truncation: None,
            effects: Vec::new(),
            data: None,
            artifact: None,
            images: Vec::new(),
            cursor: None,
            status: ToolResultStatus::Completed,
            reason: None,
            presentation: None,
        }))
    }
}

fn message_contains_text(message: &haider_provider::Message, expected: &str) -> bool {
    message
        .blocks
        .iter()
        .any(|block| matches!(block, Block::Text { text } if text == expected))
}

/// HAIDER968 LAW. MUTATION CHECKS: moving refresh back outside the logical
/// request loop makes the count and per-request snapshot assertions fail.
#[tokio::test]
async fn volatile_snapshot_refreshes_at_each_logical_request_boundary() {
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "round-1".into(),
            name: "inspect".into(),
            args: serde_json::json!({"round": 1}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "round-1".into(),
        },
        FakeStep::EmitToolCall {
            call_id: "round-2".into(),
            name: "inspect".into(),
            args: serde_json::json!({"round": 2}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "round-2".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let dispatcher = Arc::new(CountingSnapshotDispatcher {
        refreshes: AtomicUsize::new(0),
    });
    let store = Arc::new(MemoryStore::new());
    let mut config = HarnessConfig::for_session(
        SessionId::new("graph-brief-monotonic-prefix"),
        DeviceId::new("graph-brief-test"),
        1,
        1,
    );
    config.volatile_user_tail = Some("stale construction snapshot".into());
    let (actor, handle) = HarnessActor::new_with_dispatcher(
        config,
        provider.clone(),
        store,
        Some(dispatcher.clone()),
    );
    let actor_task = tokio::spawn(actor.run());

    handle
        .submit_turn(SubmitTurn::new("run two tool rounds"))
        .await
        .expect("first turn accepted")
        .wait()
        .await
        .expect("first turn completes");
    assert_eq!(
        dispatcher.refresh_count(),
        3,
        "each same-turn logical provider request refreshes its snapshot"
    );

    let first_turn_requests = provider.requests();
    assert_eq!(first_turn_requests.len(), 3);
    let first_turn_epochs = first_turn_requests
        .iter()
        .map(|request| {
            request
                .cache_metadata
                .as_ref()
                .expect("cache metadata")
                .cache_epoch
                .clone()
        })
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        first_turn_epochs.len(),
        3,
        "each refreshed logical request declares its exact-view epoch"
    );
    for (index, request) in first_turn_requests.iter().enumerate() {
        let expected = format!("GraphBrief snapshot {}", index + 1);
        let snapshot = request
            .messages
            .iter()
            .position(|message| message_contains_text(message, &expected))
            .expect("current logical-request snapshot is provider-visible");
        let current_user = request
            .messages
            .iter()
            .position(|message| message_contains_text(message, "run two tool rounds"))
            .expect("accepted current user is provider-visible");
        assert!(snapshot < current_user);
        assert_eq!(
            request
                .messages
                .iter()
                .filter(|message| message_contains_text(message, &expected))
                .count(),
            1,
            "the current snapshot is inserted exactly once per request"
        );
    }

    handle
        .submit_turn(SubmitTurn::new("start the next turn"))
        .await
        .expect("second turn accepted")
        .wait()
        .await
        .expect("second turn completes");
    assert_eq!(
        dispatcher.refresh_count(),
        4,
        "the next turn's logical request refreshes again"
    );
    let requests = provider.requests();
    assert_eq!(requests.len(), 4);
    let second_turn = &requests[3];
    assert_ne!(
        second_turn
            .cache_metadata
            .as_ref()
            .expect("second-turn cache metadata")
            .cache_epoch,
        first_turn_requests[2]
            .cache_metadata
            .as_ref()
            .expect("third request cache metadata")
            .cache_epoch,
        "the accepted turn boundary declares a new snapshot epoch"
    );
    assert!(
        second_turn
            .messages
            .iter()
            .any(|message| message_contains_text(message, "GraphBrief snapshot 4"))
    );
    assert!(
        second_turn
            .messages
            .iter()
            .all(|message| !message_contains_text(message, "GraphBrief snapshot 3"))
    );

    handle.stop().await.expect("actor stops");
    actor_task.await.expect("actor joins");
}
