#![allow(clippy::expect_used)]

use std::sync::Arc;

use haider_core::{HarnessActor, HarnessConfig, MemoryStore, SubmitTurn};
use haider_protocol::ids::{DeviceId, SessionId};
use haider_protocol::provider::{Block, FinishReason};
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

/// CG-M1 LAW: GraphBrief is provider-visible but lies strictly after both
/// cache boundaries, so activating a graph cannot change the durable-head
/// cache identity or stable-prefix token estimate.
#[tokio::test]
async fn graph_brief_is_volatile_and_cache_equivalent_to_no_active_graph() {
    let brief = "GraphBrief: VERIFY attempt 2/8; gate all-of-3; evidence 1 green/0 red (1 effective); next: record 3 green VERIFY results.";
    let (baseline, _) = one_request(None).await;
    let (active, store) = one_request(Some(brief)).await;

    assert_eq!(active.messages.len(), baseline.messages.len() + 1);
    assert_eq!(
        &active.messages[..baseline.messages.len()],
        baseline.messages
    );
    assert!(active.messages.last().is_some_and(|message| {
        matches!(message.blocks.as_slice(), [Block::Text { text }] if text == brief)
    }));
    assert_eq!(
        active.cache_metadata, baseline.cache_metadata,
        "volatile tail must not move boundaries, change prefix digests/cache epoch, or add stable tokens"
    );

    let durable = store.events(&SessionId::new("graph-brief-cache-law")).await;
    assert!(
        durable
            .iter()
            .all(|envelope| !envelope.payload.to_string().contains("GraphBrief:")),
        "GraphBrief never enters the durable prompt history"
    );
}
