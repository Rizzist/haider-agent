#![allow(clippy::expect_used)]

use super::*;
use crate::{MemoryStore, StoreHandle};
use async_trait::async_trait;
use haider_protocol::DeliveryMode;
use haider_protocol::envelope::{EventEnvelope, RenderTargets, SCHEMA_VERSION};
use haider_protocol::history::{NodeKind, TreeNode};
use haider_protocol::ids::{DeviceId, NodeId};
use haider_protocol::item::{ItemDelta, ItemEvent, TurnItem};
use haider_protocol::state::RunState;
use haider_protocol::verify::VerifyVerdict;

struct NoArtifacts;

#[tokio::test]
async fn hard_session_removal_drops_the_cache_shell() {
    let cache = PromptHistoryCache::default();
    let session_id = SessionId::new("prompt-hard-removal");
    let mut cached = CachedPromptSession::default();
    cached.envelopes.reserve(32);
    cache
        .sessions
        .lock()
        .await
        .insert(session_id.clone(), cached);

    assert_eq!(cache.retention_stats().await.sessions, 1);
    assert!(cache.remove_session(&session_id).await > 0);
    assert_eq!(cache.retention_stats().await.sessions, 0);
}

#[async_trait]
impl ArtifactReader for NoArtifacts {
    async fn read_artifact(&self, artifact: &ArtifactRef) -> Result<Vec<u8>, HaiderError> {
        Err(HaiderError::new(
            ErrorCode::StoreCorrupt,
            format!("unexpected artifact read for {artifact:?}"),
            false,
        ))
    }
}

fn pressure_envelope(session_id: &SessionId, ordinal: u64) -> RawEnvelope {
    let padding = (0..128)
        .map(|index| serde_json::json!({ "field": index }))
        .collect::<Vec<_>>();
    EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(format!("prompt-pressure-{ordinal}")),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: Some(RunId::new("prompt-pressure-source")),
        agent_id: None,
        device_id: DeviceId::new("prompt-pressure-device"),
        authority_epoch: 1,
        worker_generation: 1,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: false,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::json!({
            "type": "prompt_cache_pressure_probe",
            "padding": padding,
        })
        .into(),
    }
}

/// MUTATION CHECK: restore serialized-journal accounting or replace
/// `Vec::new()` with `clear()`. The retained estimate crosses the cap while
/// the serialized body does not, and the body-owning capacity must be gone.
#[tokio::test]
async fn retained_value_trees_evict_and_the_next_hit_recompiles() {
    const ENVELOPE_COUNT: u64 = 225;

    let store = MemoryStore::new();
    let session_id = SessionId::new("prompt-retained-heap-cap");
    let current_run = RunId::new("prompt-retained-heap-current");
    let mut envelopes = (0..ENVELOPE_COUNT)
        .map(|ordinal| pressure_envelope(&session_id, ordinal))
        .collect::<Vec<_>>();
    let mut visible = pressure_envelope(&session_id, ENVELOPE_COUNT);
    visible.run_id = Some(current_run.clone());
    visible.render.prompt = PromptRender::Verbatim;
    *visible.payload = serde_json::json!({
        "type": "user_message",
        "text": "projection rebuilt after retained-body eviction",
        "attachments": []
    });
    envelopes.push(visible);
    store
        .append(&mut envelopes)
        .await
        .expect("pressure journal appends");

    let serialized_bytes = serde_json::to_vec(&envelopes)
        .expect("pressure journal serializes")
        .len();
    let retained_bytes = envelopes
        .iter()
        .map(envelope_weight_bytes)
        .fold(0_usize, usize::saturating_add);
    assert!(serialized_bytes < PROMPT_CACHE_RETAINED_BYTES_LIMIT);
    assert!(retained_bytes > PROMPT_CACHE_RETAINED_BYTES_LIMIT);
    drop(envelopes);

    let expected = PromptHistoryCompiler::compile(&store, &session_id, None, None, &current_run)
        .await
        .expect("fresh compile succeeds");
    assert!(!expected.is_empty(), "known visible fact must project");
    let cache = PromptHistoryCache::default();
    let first = cache
        .compile_provider_projection_with_artifacts(
            &store,
            &NoArtifacts,
            &session_id,
            None,
            None,
            &current_run,
        )
        .await
        .expect("cached compile succeeds");
    assert_eq!(first.messages, expected);
    {
        let sessions = cache.sessions.lock().await;
        let cached = sessions.get(&session_id).expect("cursor shell retained");
        assert!(cached.bodies_evicted);
        assert_eq!(cached.envelopes.capacity(), 0);
        assert!(cached.append_prefixes.is_empty());
        assert!(cached.projections.is_empty());
    }

    let recompiled = cache
        .compile_provider_projection_with_artifacts(
            &store,
            &NoArtifacts,
            &session_id,
            None,
            None,
            &current_run,
        )
        .await
        .expect("evicted hit recompiles");
    assert_eq!(recompiled.messages, expected);
}

/// MUTATION CHECK: removing the quiescent-session eviction leaves decoded
/// envelope trees and compiled provider messages resident after idle.
#[tokio::test]
async fn explicit_idle_eviction_drops_bodies_but_keeps_replay_correct() {
    let store = MemoryStore::new();
    let session_id = SessionId::new("prompt-idle-release");
    let current_run = RunId::new("prompt-idle-release-run");
    let mut visible = pressure_envelope(&session_id, 1);
    visible.run_id = Some(current_run.clone());
    visible.render.prompt = PromptRender::Verbatim;
    *visible.payload = serde_json::json!({
        "type": "user_message",
        "text": "rebuild me after idle",
        "attachments": []
    });
    store
        .append(std::slice::from_mut(&mut visible))
        .await
        .expect("append visible prompt fact");

    let cache = PromptHistoryCache::default();
    let first = cache
        .compile_provider_projection_with_artifacts(
            &store,
            &NoArtifacts,
            &session_id,
            None,
            None,
            &current_run,
        )
        .await
        .expect("initial cached projection");
    let released = cache.evict_session_bodies(&session_id).await;
    assert!(released > 0, "the idle release must own measurable bodies");
    {
        let sessions = cache.sessions.lock().await;
        let cached = sessions.get(&session_id).expect("cursor shell retained");
        assert!(cached.bodies_evicted);
        assert_eq!(cached.envelopes.capacity(), 0);
    }
    let rebuilt = cache
        .compile_provider_projection_with_artifacts(
            &store,
            &NoArtifacts,
            &session_id,
            None,
            None,
            &current_run,
        )
        .await
        .expect("idle-evicted projection rebuilds");
    assert_eq!(rebuilt, first);
}

/// MUTATION CHECK: make reply canonicalization local to one 256-envelope
/// store page. The completed item and assistant node then retain independent
/// decoded strings from the delta ranges after a restart replay.
#[tokio::test]
async fn reply_arena_remains_canonical_across_history_page_boundaries() {
    fn assert_canonical(envelopes: &[RawEnvelope], expected: &str) {
        let mut ranges = Vec::new();
        for envelope in envelopes {
            match envelope
                .payload
                .decode_event()
                .expect("decode cached event")
            {
                EventPayload::Item(ItemEvent::Delta {
                    delta: ItemDelta::Text { text },
                    ..
                })
                | EventPayload::Item(ItemEvent::Completed {
                    item: TurnItem::AgentMessage { text },
                    ..
                }) => ranges.push(text),
                EventPayload::NodeCommitted(TreeNode {
                    kind: NodeKind::AssistantCommit { text, .. },
                    ..
                }) => ranges.push(text),
                _ => {}
            }
        }
        let canonical = ranges.last().expect("assistant node range");
        assert_eq!(canonical, expected);
        assert!(
            ranges
                .iter()
                .all(|range| range.shares_arena_with(canonical)),
            "every delta, completed item, and node must share one replay arena"
        );
    }

    let store = MemoryStore::new();
    let session_id = SessionId::new("prompt-reply-arena-page-boundary");
    let item_id = ItemId::new("prompt-reply-arena-item");
    let mut envelopes = Vec::new();
    let mut next_seq = 1_u64;
    let mut push_round_tripped = |payload: EventPayload| {
        let mut envelope = pressure_envelope(&session_id, next_seq);
        *envelope.payload = serde_json::to_value(payload).expect("encode reply event");
        let encoded = serde_json::to_vec(&envelope).expect("encode stored envelope");
        let decoded: RawEnvelope =
            serde_json::from_slice(&encoded).expect("decode independent stored envelope");
        envelopes.push(decoded);
        next_seq = next_seq.saturating_add(1);
    };

    push_round_tripped(EventPayload::Item(ItemEvent::Started {
        item_id: item_id.clone(),
        item: TurnItem::AgentMessage {
            text: ReplyText::default(),
        },
    }));
    for _ in 0..HISTORY_PAGE {
        push_round_tripped(EventPayload::Item(ItemEvent::Delta {
            item_id: item_id.clone(),
            delta: ItemDelta::Text { text: "x".into() },
        }));
    }
    let expected = "x".repeat(HISTORY_PAGE);
    push_round_tripped(EventPayload::Item(ItemEvent::Completed {
        item_id,
        item: TurnItem::AgentMessage {
            text: expected.clone().into(),
        },
    }));
    push_round_tripped(EventPayload::NodeCommitted(TreeNode {
        node: NodeId::new("prompt-reply-arena-node"),
        parent: None,
        kind: NodeKind::AssistantCommit {
            text: expected.clone().into(),
            verdict: VerifyVerdict::NotApplicable,
        },
    }));
    store
        .append(&mut envelopes)
        .await
        .expect("append independently decoded reply events");
    drop(envelopes);

    let uncached = read_all(&store, &session_id).await.expect("paged read_all");
    assert_canonical(&uncached, &expected);
    let head = store.latest_seq(&session_id).await.expect("journal head");
    let cached = replay_cached_session(&store, &session_id, head)
        .await
        .expect("paged cached replay");
    assert_canonical(&cached.envelopes, &expected);
    assert!(cached.active_reply_arenas.is_empty());
    assert!(cached.completed_reply_arenas.is_empty());
}

/// MUTATION CHECK: retaining the complete decoded journal at terminal idle
/// makes the post-compaction envelope count stay at the old head; dropping the
/// compiled append prefix makes the next run fall back instead of exercising
/// the bounded suffix-extension seam. Dropping the node ancestry spine makes
/// the next assistant node falsely report a missing parent.
#[tokio::test]
async fn idle_compaction_keeps_cross_run_prefix_and_replays_only_the_new_suffix() {
    let store = MemoryStore::new();
    let session_id = SessionId::new("prompt-idle-prefix-compaction");
    let first_run = RunId::new("prompt-idle-prefix-first");
    let next_run = RunId::new("prompt-idle-prefix-next");

    let mut first_user = pressure_envelope(&session_id, 1);
    first_user.run_id = Some(first_run.clone());
    first_user.render.prompt = PromptRender::Verbatim;
    *first_user.payload = serde_json::to_value(EventPayload::UserMessage {
        text: "first cached turn".into(),
        attachments: Vec::new(),
        mode: DeliveryMode::Queue,
    })
    .expect("first user payload");
    let mut first_events = vec![first_user, {
        let mut envelope = pressure_envelope(&session_id, 2);
        envelope.run_id = Some(first_run.clone());
        *envelope.payload = serde_json::to_value(EventPayload::NodeCommitted(TreeNode {
            node: NodeId::new("prompt-idle-prefix-first-user-node"),
            parent: None,
            kind: NodeKind::UserTurn {
                text: "first cached turn".into(),
                attachments: Vec::new(),
            },
        }))
        .expect("first user node payload");
        envelope
    }];
    store
        .append(&mut first_events)
        .await
        .expect("append first user and node");

    let cache = PromptHistoryCache::default();
    cache
        .compile_provider_projection_with_artifacts(
            &store,
            &NoArtifacts,
            &session_id,
            None,
            None,
            &first_run,
        )
        .await
        .expect("compile first request prefix");
    let before = cache.retention_stats().await;
    let released = cache.compact_session_history(&session_id).await;
    let compacted = cache.retention_stats().await;
    assert!(released > 0, "decoded prefix journal must be released");
    assert_eq!(compacted.envelopes, 1, "only the user-node spine remains");
    assert_eq!(compacted.append_prefixes, 1);
    assert_eq!(compacted.exact_projections, 1);
    assert!(compacted.projection_bytes > 0);
    assert!(compacted.body_bytes < before.body_bytes);

    let mut suffix = vec![
        {
            let mut envelope = pressure_envelope(&session_id, 3);
            envelope.run_id = Some(first_run.clone());
            envelope.render.prompt = PromptRender::Verbatim;
            *envelope.payload = serde_json::to_value(EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("prompt-idle-prefix-answer"),
                item: TurnItem::AgentMessage {
                    text: "answer retained through suffix extension".into(),
                },
            }))
            .expect("assistant payload");
            envelope
        },
        {
            let mut envelope = pressure_envelope(&session_id, 4);
            envelope.run_id = Some(first_run.clone());
            *envelope.payload = serde_json::to_value(EventPayload::NodeCommitted(TreeNode {
                node: NodeId::new("prompt-idle-prefix-first-answer-node"),
                parent: Some(NodeId::new("prompt-idle-prefix-first-user-node")),
                kind: NodeKind::AssistantCommit {
                    text: "answer retained through suffix extension".into(),
                    verdict: VerifyVerdict::NotApplicable,
                },
            }))
            .expect("assistant node payload");
            envelope
        },
        {
            let mut envelope = pressure_envelope(&session_id, 5);
            envelope.run_id = Some(first_run.clone());
            *envelope.payload = serde_json::to_value(EventPayload::RunState(RunState::Done))
                .expect("terminal payload");
            envelope
        },
        {
            let mut envelope = pressure_envelope(&session_id, 6);
            envelope.run_id = Some(next_run.clone());
            envelope.render.prompt = PromptRender::Verbatim;
            *envelope.payload = serde_json::to_value(EventPayload::UserMessage {
                text: "next cached turn".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            })
            .expect("next user payload");
            envelope
        },
        {
            let mut envelope = pressure_envelope(&session_id, 7);
            envelope.run_id = Some(next_run.clone());
            *envelope.payload = serde_json::to_value(EventPayload::NodeCommitted(TreeNode {
                node: NodeId::new("prompt-idle-prefix-next-user-node"),
                parent: Some(NodeId::new("prompt-idle-prefix-first-answer-node")),
                kind: NodeKind::UserTurn {
                    text: "next cached turn".into(),
                    attachments: Vec::new(),
                },
            }))
            .expect("next user node payload");
            envelope
        },
    ];
    store
        .append(&mut suffix)
        .await
        .expect("append next-run suffix");

    let expected = PromptHistoryCompiler::compile(&store, &session_id, None, None, &next_run)
        .await
        .expect("fresh suffix oracle");
    let extended = cache
        .compile_provider_projection_with_artifacts(
            &store,
            &NoArtifacts,
            &session_id,
            None,
            None,
            &next_run,
        )
        .await
        .expect("extend compacted prefix");
    assert_eq!(extended.messages, expected);
    assert!(extended.messages.iter().any(|message| {
        message.blocks.iter().any(|block| {
            matches!(block, Block::Text { text } if text == "answer retained through suffix extension")
        })
    }));
}
