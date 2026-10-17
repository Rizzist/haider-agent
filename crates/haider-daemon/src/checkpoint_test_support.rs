#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use std::path::Path;

#[cfg(feature = "android-standalone")]
pub(crate) async fn seed_recovered_checkpoint(
    store_root: &Path,
    cwd: &Path,
    label: &str,
    target: &str,
) -> haider_protocol::ids::SessionId {
    seed_checkpoint_change(
        store_root,
        cwd,
        label,
        target,
        Some(b"synthetic recovered deletion"),
        None,
    )
    .await
}

pub(crate) async fn seed_checkpoint_change(
    store_root: &Path,
    cwd: &Path,
    label: &str,
    target: &str,
    before: Option<&[u8]>,
    after: Option<&[u8]>,
) -> haider_protocol::ids::SessionId {
    use haider_core::{SqliteStoreHandle, StoreHandle};
    use haider_protocol::{
        checkpoint::{CheckpointKind, CheckpointOrigin},
        effect::{
            AuthorizationVerdict, EffectClass, EffectIntent, EffectOutcome, EffectPhase,
            WorkspaceMutation,
        },
        envelope::{EventEnvelope, RawPayload, RenderTargets, SCHEMA_VERSION},
        ids::*,
    };
    let store = SqliteStoreHandle::open(store_root)
        .await
        .expect("seed recovered store");
    let session = SessionId::new(format!("recovered-{label}"));
    store
        .create_session(haider_store::SessionCreateCommand {
            command_id: format!("seed-{label}"),
            request_digest: format!("seed-{label}"),
            request_json: "{}".into(),
            session_id: session.clone(),
            cwd: cwd.display().to_string(),
            provider: "fake".into(),
            model: "fake-model".into(),
            max_tokens: 4096,
            max_tokens_source: None,
            permission_overrides: Some(haider_protocol::session::SessionPermissionOverridesV1 {
                auto_allow: true,
                ..Default::default()
            }),
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
            event_id: EventId::new(format!("seed-created-{label}")),
            device_id: DeviceId::new("synthetic-device"),
        })
        .await
        .expect("recovered full-trust session");
    let run = RunId::new(format!("seed-run-{label}"));
    let mut cas = store.clone();
    let checkpoint = haider_tools::freeze_checkpoint(
        &mut cas,
        haider_tools::FreezeCheckpointInput {
            session_id: session.clone(),
            branch_id: None,
            run_id: run.clone(),
            effect_id: EffectId::new(format!("seed-effect-{label}")),
            call_id: label.into(),
            origin: CheckpointOrigin::Tool,
            source_checkpoint_id: None,
        },
        haider_tools::CheckpointCapture {
            kind: match (before, after) {
                (None, Some(_)) => CheckpointKind::Create,
                (Some(_), None) => CheckpointKind::Delete,
                _ => CheckpointKind::Write,
            },
            paths: vec![haider_tools::CheckpointCapturePath {
                path: target.into(),
                pre_bytes: before.map(<[u8]>::to_vec),
                pre_digest: before.map(|b| format!("blake3:{}", blake3::hash(b).to_hex())),
                post_digest: after.map(|b| format!("blake3:{}", blake3::hash(b).to_hex())),
                truncated_reason: None,
            }],
            post_digest: format!("blake3:{}", blake3::hash(label.as_bytes()).to_hex()),
            redacted_content: false,
        },
    )
    .await
    .expect("checkpoint with durable preimage");
    // Recovery starts from a valid durable effect history. The store stamps
    // the checkpoint's revision from its preceding mutation outcome.
    let effect = checkpoint.effect_id.clone();
    let payloads = [
        EventPayload::Effect(EffectPhase::Intent(EffectIntent {
            effect: effect.clone(),
            class: EffectClass::FsWrite,
            summary: "synthetic checkpoint mutation".into(),
            args_digest: checkpoint.post_digest.clone(),
            workspace_revision: None,
        })),
        EventPayload::Effect(EffectPhase::Authorized {
            effect: effect.clone(),
            verdict: AuthorizationVerdict::Allow,
        }),
        EventPayload::Effect(EffectPhase::Dispatched {
            effect: effect.clone(),
        }),
        EventPayload::Effect(EffectPhase::Outcome {
            effect: effect.clone(),
            outcome: EffectOutcome::Ok,
            freshness: None,
            workspace_mutation: Some(WorkspaceMutation {
                effect_id: effect,
                mutation_digest: checkpoint.post_digest.clone(),
                workspace_revision: None,
                subject_digest: None,
                redacted_content: false,
            }),
        }),
        EventPayload::CheckpointRecorded(checkpoint),
    ];
    let mut events: Vec<_> = payloads
        .into_iter()
        .enumerate()
        .map(|(ordinal, payload)| EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: EventId::new(format!("seed-checkpoint-{label}-{ordinal}")),
            seq: 0,
            session_id: session.clone(),
            branch_id: None,
            run_id: Some(run.clone()),
            agent_id: None,
            device_id: DeviceId::new("synthetic-device"),
            authority_epoch: 0,
            worker_generation: store.worker_generation(),
            causation_id: None,
            correlation_id: None,
            committed_at_ms: 0,
            render: RenderTargets {
                ui: false,
                durable: true,
                prompt: haider_protocol::envelope::PromptRender::Omit,
            },
            payload: RawPayload::from_event(payload).expect("checkpoint payload"),
        })
        .collect();
    store
        .append(&mut events)
        .await
        .expect("recovered checkpoint journal");
    assert_eq!(
        store
            .list_checkpoints(session.clone(), None, None, 10)
            .await
            .expect("indexed checkpoint")
            .checkpoints
            .len(),
        1
    );
    if let Some(bytes) = after {
        std::fs::write(cwd.join(target), bytes).expect("durable postimage on disk");
    }
    drop(cas);
    store.close().await.expect("close seed");
    session
}
