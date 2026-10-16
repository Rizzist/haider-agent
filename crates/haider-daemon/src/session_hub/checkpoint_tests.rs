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

#[cfg(unix)]
#[derive(Default)]
struct CheckpointSink(std::sync::Mutex<Vec<WireFrame>>);
#[cfg(unix)]
impl FrameSink for CheckpointSink {
    fn try_send(&self, frame: WireFrame) -> Result<(), FrameSendError> {
        self.0.lock().expect("checkpoint frames").push(frame);
        Ok(())
    }
}

#[cfg(unix)]
async fn commit_checkpoint_through_store(origin: CheckpointOrigin) {
    use crate::checkpoint_test_support::seed_checkpoint_change;
    use haider_core::{SqliteStoreHandle, StoreHandle};
    // All three commands start from a durable agent mutation, including CAS
    // preimage and revision. Reopening exercises recovery rather than an
    // envelope-only simulation. Android uses the same fixture and RPC door.
    let old = b"synthetic preimage".as_slice();
    let new = b"synthetic postimage".as_slice();
    for (before_bytes, after_bytes) in
        [(None, Some(new)), (Some(old), None), (Some(old), Some(new))]
    {
        let root = tempfile::tempdir().expect("checkpoint profile");
        #[cfg(feature = "android-standalone")]
        let workspace =
            tempfile::tempdir_in(crate::android_workspace::test_root()).expect("Android workspace");
        #[cfg(not(feature = "android-standalone"))]
        let workspace = tempfile::tempdir().expect("workspace");
        let session = seed_checkpoint_change(
            root.path(),
            workspace.path(),
            "default",
            "deleted.txt",
            before_bytes,
            after_bytes,
        )
        .await;
        let store = SqliteStoreHandle::open(root.path())
            .await
            .expect("reopen store");
        let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
        let sink = Arc::new(CheckpointSink::default());
        let connection = hub
            .open_connection(
                BTreeSet::from([
                    haider_rpc::Capability::View,
                    haider_rpc::Capability::Control,
                ]),
                sink.clone(),
                crate::accounts::ConnectionTransport::LocalSameUid,
            )
            .expect("control connection");
        connection
            .request(
                RequestId::new("attach"),
                serde_json::from_value(serde_json::json!({
                    "method":"session.attach", "session_id":session, "after_seq":0, "mode":"control"
                }))
                .expect("attach request"),
            )
            .await
            .expect("attach");
        assert!(sink.0.lock().expect("frames").iter().any(|f| matches!(
            f,
            WireFrame::Response {
                body: ResponseBody::SessionAttach { .. },
                ..
            }
        )));
        let before = store.read(&session, 0, 100).await.expect("seed journal");
        let mut tracker = haider_protocol::activity::ActivityTracker::new();
        for envelope in &before {
            tracker.observe_raw(envelope);
        }
        let prior = tracker.state();
        assert_eq!(
            tracker.used(),
            vec![haider_protocol::activity::ActivityKind::Edit]
        );
        let target = workspace.path().join("deleted.txt");
        assert_eq!(std::fs::read(&target).ok().as_deref(), after_bytes);
        let commands = if origin == CheckpointOrigin::Redo {
            vec![CheckpointOrigin::Undo, origin]
        } else {
            vec![origin]
        };
        for action in commands {
            let method = match action {
                CheckpointOrigin::Undo => "checkpoint.undo",
                CheckpointOrigin::Redo => "checkpoint.redo",
                CheckpointOrigin::RollbackTurn => "checkpoint.rollback_turn",
                CheckpointOrigin::Tool => unreachable!(),
            };
            let mut body = serde_json::json!({"method":method, "command_id":method,
            "session_id":session, "worker_generation":store.worker_generation()});
            if action == CheckpointOrigin::RollbackTurn {
                body["run_id"] = "seed-run-default".into();
            } else {
                body["target"] = "last".into();
            }
            let request: RequestBody = serde_json::from_value(body).expect("checkpoint request");
            let head = store.latest_seq(&session).await.expect("head");
            for attempt in 0..2 {
                let id = RequestId::new(format!("{method}-{attempt}"));
                connection
                    .request(id.clone(), request.clone())
                    .await
                    .expect("checkpoint RPC");
                let frames = sink.0.lock().expect("frames");
                let response = frames
                    .iter()
                    .find_map(|f| match f {
                        WireFrame::Response { request_id, body } if request_id == &id => Some(body),
                        _ => None,
                    })
                    .expect("checkpoint response");
                assert!(
                    matches!(
                        (action, response),
                        (CheckpointOrigin::Undo, ResponseBody::CheckpointUndo { .. })
                            | (CheckpointOrigin::Redo, ResponseBody::CheckpointRedo { .. })
                            | (
                                CheckpointOrigin::RollbackTurn,
                                ResponseBody::CheckpointRollbackTurn { .. }
                            )
                    ),
                    "{response:?}"
                );
            }
            assert_eq!(
                store.latest_seq(&session).await.expect("committed head"),
                head + 5,
                "receipt replay cannot restore twice or append twice"
            );
            let expected = if action == CheckpointOrigin::Redo {
                after_bytes
            } else {
                before_bytes
            };
            assert_eq!(
                std::fs::read(&target).ok().as_deref(),
                expected,
                "{action:?} file restoration"
            );
            let committed = store
                .read(&session, head, 100)
                .await
                .expect("restore journal");
            assert_eq!(committed.len(), 5);
            assert!(matches!(
                committed[0].payload.decode_event().expect("typed intent"),
                EventPayload::Effect(EffectPhase::Intent(EffectIntent {
                    class: EffectClass::CheckpointRestore,
                    ..
                }))
            ));
            assert!(
                matches!(committed[4].payload.decode_event().expect("checkpoint receipt"),
            EventPayload::CheckpointRecorded(ref c) if c.origin == action && c.workspace_revision.is_some())
            );
            for envelope in &committed {
                tracker.observe_raw(envelope);
            }
            assert_eq!(
                tracker.state(),
                prior,
                "operator restore leaves agent activity and history unchanged"
            );
            let mut replay = haider_protocol::activity::ActivityTracker::new();
            for envelope in store.read(&session, 0, 100).await.expect("full journal") {
                replay.observe_raw(&envelope);
            }
            assert_eq!(
                replay.state(),
                prior,
                "restart projection preserves agent history"
            );
        }
        connection.close().await.expect("close connection");
        hub.shutdown().await.expect("shutdown hub");
        store.close().await.expect("close store");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn checkpoint_undo_commits_restore_and_receipt_through_real_store() {
    commit_checkpoint_through_store(CheckpointOrigin::Undo).await;
}
#[cfg(unix)]
#[tokio::test]
async fn checkpoint_redo_commits_restore_and_receipt_through_real_store() {
    commit_checkpoint_through_store(CheckpointOrigin::Redo).await;
}
#[cfg(unix)]
#[tokio::test]
async fn checkpoint_rollback_turn_commits_restore_and_receipt_through_real_store() {
    commit_checkpoint_through_store(CheckpointOrigin::RollbackTurn).await;
}
