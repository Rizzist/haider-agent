#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::checkpoint::{
    CheckpointCursor, CheckpointKind, CheckpointOrigin, CheckpointPath, CheckpointRecorded,
};
use haider_protocol::effect::{
    EffectClass, EffectIntent, EffectOutcome, EffectPhase, WorkspaceMutation,
};
use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION,
};
use haider_protocol::ids::{CheckpointId, DeviceId, EffectId, EventId, RunId, SessionId};
use haider_store::{EventStore, SessionCreateCommand, Store};

fn create_session(store: &Store, session_id: &SessionId) {
    store
        .create_session(&SessionCreateCommand {
            command_id: "checkpoint-create-session".into(),
            request_digest: "checkpoint-create-digest".into(),
            request_json: r#"{"session":"checkpoint-projection"}"#.into(),
            session_id: session_id.clone(),
            cwd: "/tmp".into(),
            provider: "fake".into(),
            model: "fake-v1".into(),
            max_tokens: 4096,
            permission_overrides: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: "checkpoint-test-v1".into(),
            event_id: EventId::new("checkpoint-session-created"),
            device_id: DeviceId::new("checkpoint-test-device"),
        })
        .expect("create checkpoint test session");
}

fn envelope(
    store: &Store,
    session_id: &SessionId,
    run_id: &RunId,
    event_id: &str,
    payload: EventPayload,
) -> RawEnvelope {
    EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(event_id),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: Some(run_id.clone()),
        agent_id: None,
        device_id: DeviceId::new("checkpoint-test-device"),
        authority_epoch: 0,
        worker_generation: store.worker_generation(),
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::to_value(payload).expect("encode checkpoint event"),
    }
}

#[test]
fn checkpoint_fact_is_stamped_and_projected_in_the_effect_transaction() {
    let root = tempfile::tempdir().expect("temporary store");
    let store = Store::open(root.path()).expect("open store");
    let session_id = SessionId::new("checkpoint-projection-session");
    create_session(&store, &session_id);
    let run_id = RunId::new("checkpoint-projection-run");
    let effect_id = EffectId::new("checkpoint-projection-effect");
    let mutation_digest = "blake3:checkpoint-projection-post".to_owned();
    let checkpoint_id = CheckpointId::new("checkpoint-projection-id");
    let mut events = vec![
        envelope(
            &store,
            &session_id,
            &run_id,
            "checkpoint-intent",
            EventPayload::Effect(EffectPhase::Intent(EffectIntent {
                effect: effect_id.clone(),
                class: EffectClass::FsWrite,
                summary: "checkpoint projection test".into(),
                args_digest: "blake3:checkpoint-projection-args".into(),
                workspace_revision: None,
            })),
        ),
        envelope(
            &store,
            &session_id,
            &run_id,
            "checkpoint-outcome",
            EventPayload::Effect(EffectPhase::Outcome {
                effect: effect_id.clone(),
                outcome: EffectOutcome::Ok,
                freshness: None,
                workspace_mutation: Some(WorkspaceMutation {
                    effect_id: effect_id.clone(),
                    mutation_digest: mutation_digest.clone(),
                    workspace_revision: None,
                    subject_digest: None,
                }),
            }),
        ),
        envelope(
            &store,
            &session_id,
            &run_id,
            "checkpoint-recorded",
            EventPayload::CheckpointRecorded(CheckpointRecorded {
                checkpoint_id: checkpoint_id.clone(),
                session_id: session_id.clone(),
                branch_id: None,
                run_id: run_id.clone(),
                effect_id,
                call_id: "checkpoint-projection-call".into(),
                seq: 0,
                workspace_revision: None,
                kind: CheckpointKind::Create,
                origin: CheckpointOrigin::Tool,
                source_checkpoint_id: None,
                paths: vec![CheckpointPath {
                    path: "created.txt".into(),
                    pre_artifact: None,
                    pre_digest: None,
                    post_digest: Some("blake3:created".into()),
                    truncated_reason: None,
                }],
                post_digest: mutation_digest,
                recorded_at_ms: 0,
            }),
        ),
    ];

    store.append(&mut events).expect("append checkpoint batch");
    let EventPayload::CheckpointRecorded(stamped) =
        serde_json::from_value(events[2].payload.clone()).expect("decode stamped checkpoint")
    else {
        panic!("checkpoint fact remains present");
    };
    assert_eq!(stamped.seq, events[2].seq);
    assert!(stamped.workspace_revision.is_some());
    assert_eq!(stamped.recorded_at_ms, events[2].committed_at_ms);

    let page = store
        .list_checkpoints(&session_id, None, None, 1)
        .expect("list checkpoint projection");
    assert_eq!(page.checkpoints, vec![stamped.clone()]);
    assert!(page.next_cursor.is_none());
    assert_eq!(
        store
            .checkpoint(&session_id, &checkpoint_id)
            .expect("lookup checkpoint"),
        Some(stamped)
    );
    let empty = store
        .list_checkpoints(&session_id, None, Some(&CheckpointCursor(events[2].seq)), 1)
        .expect("list older checkpoint page");
    assert!(empty.checkpoints.is_empty());
}
