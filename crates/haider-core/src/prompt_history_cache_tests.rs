#![allow(clippy::expect_used)]

use super::*;
use crate::{MemoryStore, StoreHandle};
use async_trait::async_trait;
use haider_protocol::envelope::{EventEnvelope, RenderTargets, SCHEMA_VERSION};
use haider_protocol::ids::DeviceId;

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
