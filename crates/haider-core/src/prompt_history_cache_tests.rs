#![allow(clippy::expect_used)]

use super::*;
use crate::{MemoryStore, StoreHandle};
use async_trait::async_trait;
use haider_protocol::DeliveryMode;
use haider_protocol::envelope::{EventEnvelope, RenderTargets, SCHEMA_VERSION};
use haider_protocol::history::{NodeKind, TreeNode};
use haider_protocol::ids::{DeviceId, NodeId};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::state::RunState;
use haider_protocol::verify::VerifyVerdict;

struct NoArtifacts;

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
        }),
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
    visible.payload = serde_json::json!({
        "type": "user_message",
        "text": "projection rebuilt after retained-body eviction",
        "attachments": []
    });
    envelopes.push(visible);
    store
        .append(&mut envelopes)
        .await
        .expect("pressure journal appends");

    let serialized_bytes = serialized_body_bytes(&envelopes);
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
    visible.payload = serde_json::json!({
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
    first_user.payload = serde_json::to_value(EventPayload::UserMessage {
        text: "first cached turn".into(),
        attachments: Vec::new(),
        mode: DeliveryMode::Queue,
    })
    .expect("first user payload");
    let mut first_events = vec![first_user, {
        let mut envelope = pressure_envelope(&session_id, 2);
        envelope.run_id = Some(first_run.clone());
        envelope.payload = serde_json::to_value(EventPayload::NodeCommitted(TreeNode {
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
            envelope.payload = serde_json::to_value(EventPayload::Item(ItemEvent::Completed {
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
            envelope.payload = serde_json::to_value(EventPayload::NodeCommitted(TreeNode {
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
            envelope.payload = serde_json::to_value(EventPayload::RunState(RunState::Done))
                .expect("terminal payload");
            envelope
        },
        {
            let mut envelope = pressure_envelope(&session_id, 6);
            envelope.run_id = Some(next_run.clone());
            envelope.render.prompt = PromptRender::Verbatim;
            envelope.payload = serde_json::to_value(EventPayload::UserMessage {
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
            envelope.payload = serde_json::to_value(EventPayload::NodeCommitted(TreeNode {
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
