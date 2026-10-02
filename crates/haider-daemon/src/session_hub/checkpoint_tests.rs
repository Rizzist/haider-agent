#![allow(clippy::expect_used)]

use super::*;
use haider_protocol::checkpoint::{
    CheckpointKind, CheckpointOrigin, CheckpointPath, CheckpointRecorded,
};
use haider_protocol::ids::{CheckpointId, EffectId};

#[test]
fn checkpoint_command_effect_batch_keeps_outcome_before_checkpoint() {
    for origin in [
        CheckpointOrigin::Undo,
        CheckpointOrigin::Redo,
        CheckpointOrigin::RollbackTurn,
    ] {
        let session_id = SessionId::new("session-checkpoint-order");
        let run_id = RunId::new("run-checkpoint-order");
        let effect_id = EffectId::new("effect-checkpoint-order");
        let checkpoint = CheckpointRecorded {
            checkpoint_id: CheckpointId::new("checkpoint-order"),
            session_id: session_id.clone(),
            branch_id: None,
            run_id: run_id.clone(),
            effect_id: effect_id.clone(),
            call_id: "command-order".into(),
            seq: 0,
            workspace_revision: None,
            kind: CheckpointKind::Write,
            origin,
            source_checkpoint_id: None,
            paths: vec![CheckpointPath {
                path: "src/lib.rs".into(),
                pre_artifact: None,
                pre_digest: None,
                post_digest: None,
                truncated_reason: None,
            }],
            post_digest: "blake3:checkpoint-order".into(),
            recorded_at_ms: 0,
            redacted_content: false,
        };
        let envelopes = checkpoint_effect_envelopes(CheckpointEffectEnvelopeInput {
            session_id,
            branch_id: None,
            run_id,
            effect_id,
            command_id: "command-order",
            mutation_digest: "blake3:checkpoint-order".into(),
            checkpoint,
            worker_generation: 7,
            device_id: &DeviceId::new("device-order"),
        })
        .expect("checkpoint effect batch serializes");

        assert_eq!(envelopes.len(), 5);
        assert!(
            matches!(
                serde_json::from_value::<EventPayload>(envelopes[0].payload.clone().into())
                    .expect("intent decodes"),
                EventPayload::Effect(EffectPhase::Intent(EffectIntent {
                    class: EffectClass::CheckpointRestore,
                    ..
                }))
            ),
            "{origin:?} is an operator checkpoint, not an agent FsWrite"
        );
        assert!(matches!(
            serde_json::from_value::<EventPayload>(envelopes[3].payload.clone().into())
                .expect("outcome decodes"),
            EventPayload::Effect(EffectPhase::Outcome { .. })
        ));
        assert!(matches!(
            serde_json::from_value::<EventPayload>(envelopes[4].payload.clone().into())
                .expect("checkpoint decodes"),
            EventPayload::CheckpointRecorded(_)
        ));
        let mut live = haider_protocol::activity::ActivityTracker::new();
        let mut replay = haider_protocol::activity::ActivityTracker::new();
        for envelope in envelopes {
            live.observe_raw(&envelope);
            let persisted = serde_json::to_vec(&envelope).expect("journal bytes");
            replay.observe_raw(&serde_json::from_slice(&persisted).expect("restart journal"));
            assert!(live.used().is_empty(), "{origin:?} live history");
            assert!(replay.used().is_empty(), "{origin:?} replay history");
            assert_eq!(live.current(0, false), None, "{origin:?} live icon");
            assert_eq!(replay.current(0, false), None, "{origin:?} replay icon");
        }
    }
}
