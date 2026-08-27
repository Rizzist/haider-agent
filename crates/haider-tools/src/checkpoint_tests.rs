#![allow(clippy::expect_used)]

use super::*;
use async_trait::async_trait;
use haider_protocol::checkpoint::{
    CHECKPOINT_PREIMAGE_MAX_BYTES, CheckpointKind, CheckpointOrigin,
};
use haider_protocol::ids::{ArtifactRef, EffectId, RunId, SessionId};
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct RecordingCas {
    stored: Vec<Vec<u8>>,
    batch_calls: usize,
}

#[async_trait]
impl CasSink for RecordingCas {
    async fn put(&mut self, bytes: &[u8]) -> ToolResult<ArtifactRef> {
        self.stored.push(bytes.to_vec());
        Ok(ArtifactRef::new(format!(
            "blake3:{}",
            blake3::hash(bytes).to_hex()
        )))
    }

    async fn put_batch(&mut self, blobs: &[Vec<u8>]) -> ToolResult<Vec<ArtifactRef>> {
        self.batch_calls = self.batch_calls.saturating_add(1);
        let mut artifacts = Vec::with_capacity(blobs.len());
        for bytes in blobs {
            artifacts.push(self.put(bytes).await?);
        }
        Ok(artifacts)
    }

    async fn put_file(&mut self, path: &Path) -> ToolResult<ArtifactRef> {
        let bytes = fs::read(path)
            .map_err(|error| ToolError::io("read checkpoint test artifact", path, error))?;
        self.put(&bytes).await
    }
}

#[derive(Clone, Default)]
struct SharedCas(Arc<Mutex<Vec<Vec<u8>>>>);

#[async_trait]
impl CasSink for SharedCas {
    async fn put(&mut self, bytes: &[u8]) -> ToolResult<ArtifactRef> {
        self.0
            .lock()
            .map_err(|_| ToolError::cas("shared checkpoint CAS lock is poisoned"))?
            .push(bytes.to_vec());
        Ok(ArtifactRef::new(format!(
            "blake3:{}",
            blake3::hash(bytes).to_hex()
        )))
    }

    async fn put_file(&mut self, path: &Path) -> ToolResult<ArtifactRef> {
        let bytes = fs::read(path)
            .map_err(|error| ToolError::io("read shared checkpoint CAS input", path, error))?;
        self.put(&bytes).await
    }
}

#[derive(Clone, Default)]
struct JournalObserver(Arc<Mutex<Vec<haider_protocol::EventPayload>>>);

struct AtomicRecordingJournal(JournalObserver);

struct NonCheckpointJournal;

#[async_trait]
impl JournalSink for NonCheckpointJournal {
    async fn append(&mut self, _payload: haider_protocol::EventPayload) -> ToolResult<()> {
        Ok(())
    }
}

#[async_trait]
impl JournalSink for AtomicRecordingJournal {
    async fn append(&mut self, payload: haider_protocol::EventPayload) -> ToolResult<()> {
        self.0
            .0
            .lock()
            .map_err(|_| ToolError::journal("checkpoint journal lock is poisoned"))?
            .push(payload);
        Ok(())
    }

    fn supports_checkpoint_batches(&self) -> bool {
        true
    }

    async fn append_checkpointed(
        &mut self,
        outcome: haider_protocol::EventPayload,
        checkpoint: haider_protocol::EventPayload,
    ) -> ToolResult<()> {
        self.0
            .0
            .lock()
            .map_err(|_| ToolError::journal("checkpoint journal lock is poisoned"))?
            .extend([outcome, checkpoint]);
        Ok(())
    }
}

fn freeze_input() -> FreezeCheckpointInput {
    FreezeCheckpointInput {
        session_id: SessionId::new("checkpoint-test-session"),
        branch_id: None,
        run_id: RunId::new("checkpoint-test-run"),
        effect_id: EffectId::new("checkpoint-test-effect"),
        call_id: "checkpoint-test-call".into(),
        origin: CheckpointOrigin::Tool,
        source_checkpoint_id: None,
    }
}

#[tokio::test]
async fn freeze_checkpoint_marks_absent_and_over_limit_preimages_explicitly() {
    let oversized_len = usize::try_from(CHECKPOINT_PREIMAGE_MAX_BYTES)
        .expect("checkpoint cap fits the test platform")
        + 1;
    let bounded = b"before".to_vec();
    let bounded_digest = format!("blake3:{}", blake3::hash(&bounded).to_hex());
    let mut cas = RecordingCas::default();
    let checkpoint = freeze_checkpoint(
        &mut cas,
        freeze_input(),
        CheckpointCapture {
            kind: CheckpointKind::Write,
            paths: vec![
                CheckpointCapturePath {
                    path: "bounded.txt".into(),
                    pre_bytes: Some(bounded.clone()),
                    pre_digest: Some(bounded_digest.clone()),
                    post_digest: Some("blake3:post-bounded".into()),
                    truncated_reason: None,
                },
                CheckpointCapturePath {
                    path: "created.txt".into(),
                    pre_bytes: None,
                    pre_digest: None,
                    post_digest: Some("blake3:post-created".into()),
                    truncated_reason: None,
                },
                CheckpointCapturePath {
                    path: "large.bin".into(),
                    pre_bytes: Some(vec![0; oversized_len]),
                    pre_digest: Some("blake3:pre-large".into()),
                    post_digest: Some("blake3:post-large".into()),
                    truncated_reason: None,
                },
            ],
            post_digest: "blake3:aggregate".into(),
        },
    )
    .await
    .expect("freeze checkpoint");

    assert_eq!(cas.stored, vec![bounded]);
    assert_eq!(cas.batch_calls, 1, "all bounded pre-images share one batch");
    assert!(checkpoint.paths[0].pre_artifact.is_some());
    assert_eq!(
        checkpoint.paths[0].pre_digest.as_deref(),
        Some(bounded_digest.as_str())
    );
    assert!(checkpoint.paths[1].pre_artifact.is_none());
    assert!(checkpoint.paths[1].pre_digest.is_none());
    assert!(checkpoint.paths[1].truncated_reason.is_none());
    assert!(checkpoint.paths[2].pre_artifact.is_none());
    assert!(
        checkpoint.paths[2]
            .truncated_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("checkpoint limit"))
    );
}

#[tokio::test]
async fn filesystem_checkpoint_freezes_preimage_before_atomic_terminal_publication() {
    let root = tempfile::tempdir().expect("temporary workspace");
    fs::write(root.path().join("tracked.txt"), b"before").expect("seed tracked file");
    let observer = JournalObserver::default();
    let mut broker = EffectBroker::new(
        Box::new(AtomicRecordingJournal(observer.clone())),
        root.path(),
        SessionId::new("checkpoint-fs-session"),
        11,
    )
    .expect("create filesystem broker");
    broker
        .restore_freshness([haider_protocol::effect::FileFreshness {
            path: "tracked.txt".into(),
            digest: format!("blake3:{}", blake3::hash(b"before").to_hex()),
        }])
        .expect("restore tracked-file freshness");
    let mut policy = PermissionPolicy::default();
    policy.allow(haider_protocol::effect::EffectClass::FsWrite);
    let ledger = ChangeLedger::new();
    let cas = SharedCas::default();
    let cas_observer = cas.clone();
    broker
        .fs_write_checkpointed(
            &FsWrite::new("tracked.txt", "after"),
            &policy,
            &TurnAttribution::new(
                SessionId::new("checkpoint-fs-session"),
                RunId::new("checkpoint-fs-run"),
            )
            .with_tool_call(None, "checkpoint-fs-call"),
            &ledger,
            cas,
        )
        .await
        .expect("checkpointed write");

    assert_eq!(
        *cas_observer.0.lock().expect("checkpoint CAS observation"),
        vec![b"before".to_vec()]
    );
    let payloads = observer.0.lock().expect("checkpoint journal observation");
    assert_eq!(payloads.len(), 5);
    assert!(matches!(
        payloads[3],
        haider_protocol::EventPayload::Effect(haider_protocol::effect::EffectPhase::Outcome { .. })
    ));
    let haider_protocol::EventPayload::CheckpointRecorded(checkpoint) = &payloads[4] else {
        panic!("terminal checkpoint pair must end with CheckpointRecorded");
    };
    assert_eq!(checkpoint.call_id, "checkpoint-fs-call");
    assert_eq!(checkpoint.paths[0].path, "tracked.txt");
    assert!(checkpoint.paths[0].pre_artifact.is_some());
    assert_eq!(
        fs::read(root.path().join("tracked.txt")).expect("read result"),
        b"after"
    );
}

#[tokio::test]
async fn unsupported_checkpoint_sink_fails_before_filesystem_mutation() {
    let root = tempfile::tempdir().expect("temporary workspace");
    let tracked = root.path().join("tracked.txt");
    fs::write(&tracked, b"before").expect("seed tracked file");
    let mut broker = EffectBroker::new(
        Box::new(NonCheckpointJournal),
        root.path(),
        SessionId::new("checkpoint-capability-session"),
        12,
    )
    .expect("create filesystem broker");
    broker
        .restore_freshness([haider_protocol::effect::FileFreshness {
            path: "tracked.txt".into(),
            digest: format!("blake3:{}", blake3::hash(b"before").to_hex()),
        }])
        .expect("restore tracked-file freshness");
    let mut policy = PermissionPolicy::default();
    policy.allow(haider_protocol::effect::EffectClass::FsWrite);
    let error = broker
        .fs_write(
            &FsWrite::new("tracked.txt", "after"),
            &policy,
            &TurnAttribution::new(
                SessionId::new("checkpoint-capability-session"),
                RunId::new("checkpoint-capability-run"),
            ),
            &ChangeLedger::new(),
        )
        .await
        .expect_err("unsupported sink must reject the mutation");
    assert!(matches!(error, ToolError::Journal { .. }));
    assert_eq!(fs::read(tracked).expect("read tracked file"), b"before");
}

#[test]
fn restore_preflights_every_path_before_changing_any_file() {
    let root = tempfile::tempdir().expect("temporary workspace");
    let first = root.path().join("first.txt");
    let second = root.path().join("second.txt");
    fs::write(&first, b"current-first").expect("seed first");
    fs::write(&second, b"foreign-second").expect("seed second");
    let first_digest = format!("blake3:{}", blake3::hash(b"current-first").to_hex());
    let plan = CheckpointRestorePlan {
        workspace_root: root.path().to_path_buf(),
        targets: vec![
            CheckpointRestoreTarget {
                path: "first.txt".into(),
                expected_digest: Some(first_digest),
                restore_bytes: Some(b"restored-first".to_vec()),
            },
            CheckpointRestoreTarget {
                path: "second.txt".into(),
                expected_digest: Some("blake3:not-current".into()),
                restore_bytes: Some(b"restored-second".to_vec()),
            },
        ],
    };

    let error = restore_checkpoint_plan(&plan).expect_err("foreign edit must conflict");
    assert!(matches!(error, CheckpointRestoreError::Conflict(_)));
    assert_eq!(fs::read(&first).expect("read first"), b"current-first");
    assert_eq!(fs::read(&second).expect("read second"), b"foreign-second");
}
