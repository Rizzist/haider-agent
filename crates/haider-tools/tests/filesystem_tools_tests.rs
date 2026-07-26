#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::effect::{EffectClass, EffectOutcome, EffectPhase};
use haider_protocol::ids::{ArtifactRef, RunId, SessionId};
use haider_tools::{
    CasSink, ChangeLedger, EffectBroker, FsList, FsPatch, FsRead, FsSearch, JournalSink,
    PermissionPolicy, ResultBounds, ToolError, ToolResult, TurnAttribution,
};
use std::fs;

#[derive(Debug, Default)]
struct RecordingJournal {
    payloads: Vec<EventPayload>,
}

#[async_trait::async_trait]
impl JournalSink for RecordingJournal {
    async fn append(&mut self, payload: EventPayload) -> ToolResult<()> {
        self.payloads.push(payload);
        Ok(())
    }
}

#[derive(Debug, Default)]
struct RecordingCas {
    writes: Vec<Vec<u8>>,
}

#[async_trait::async_trait]
impl CasSink for RecordingCas {
    async fn put(&mut self, bytes: &[u8]) -> ToolResult<ArtifactRef> {
        self.writes.push(bytes.to_vec());
        Ok(ArtifactRef::new(format!(
            "blake3:{}",
            blake3::hash(bytes).to_hex()
        )))
    }
}

fn allow(class: EffectClass) -> PermissionPolicy {
    let mut policy = PermissionPolicy::default();
    policy.allow(class);
    policy
}

#[tokio::test]
async fn preimage_mismatch_returns_typed_conflict_and_failed_outcome() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("conflict.txt");
    fs::write(&path, "current").expect("seed file");
    let mut broker = EffectBroker::new(RecordingJournal::default());
    let attribution = TurnAttribution::new(SessionId::new("session"), RunId::new("turn"));
    let mut ledger = ChangeLedger::new();

    let error = broker
        .fs_patch(
            &FsPatch::new(&path, "stale", "replacement"),
            &allow(EffectClass::FsWrite),
            &attribution,
            &mut ledger,
        )
        .await
        .expect_err("preimage mismatch");

    let ToolError::Conflict(conflict) = error else {
        panic!("expected typed conflict");
    };
    assert_eq!(conflict.path, path);
    assert_eq!(conflict.expected_preimage, "stale");
    assert_eq!(fs::read_to_string(&path).expect("read file"), "current");
    assert!(!ledger.has_fs_writes(&attribution.session, &attribution.turn));
    assert!(matches!(
        broker.journal().payloads.last(),
        Some(EventPayload::Effect(EffectPhase::Outcome {
            outcome: EffectOutcome::Failed { .. },
            ..
        }))
    ));
}

#[tokio::test]
async fn ledger_attributes_successful_writes_to_the_exact_turn() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let first_path = directory.path().join("first.txt");
    let second_path = directory.path().join("second.txt");
    fs::write(&first_path, "a").expect("seed first");
    fs::write(&second_path, "x").expect("seed second");
    let session = SessionId::new("session-ledger");
    let first_turn = TurnAttribution::new(session.clone(), RunId::new("turn-1"));
    let second_turn = TurnAttribution::new(session.clone(), RunId::new("turn-2"));
    let policy = allow(EffectClass::FsWrite);
    let mut broker = EffectBroker::new(RecordingJournal::default());
    let mut ledger = ChangeLedger::new();

    broker
        .fs_patch(
            &FsPatch::new(&first_path, "a", "b"),
            &policy,
            &first_turn,
            &mut ledger,
        )
        .await
        .expect("first patch");
    broker
        .fs_patch(
            &FsPatch::new(&second_path, "x", "y"),
            &policy,
            &second_turn,
            &mut ledger,
        )
        .await
        .expect("second patch");

    let first = ledger
        .changes_for(&session, &first_turn.turn)
        .expect("first turn changes");
    assert_eq!(first.paths, vec![first_path.clone()]);
    assert_eq!(first.writes.len(), 1);
    assert!(ledger.path_touched(&session, &first_turn.turn, &first_path));
    assert!(!ledger.path_touched(&session, &first_turn.turn, &second_path));
    assert_eq!(
        ledger
            .changes_for(&session, &second_turn.turn)
            .expect("second turn changes")
            .paths,
        vec![second_path]
    );
}

#[tokio::test]
async fn oversized_result_keeps_preview_and_freezes_full_payload_in_cas() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("large.txt");
    let full = "αβγ delta\n".repeat(16);
    fs::write(&path, &full).expect("seed file");
    let mut broker = EffectBroker::new(RecordingJournal::default());
    let mut cas = RecordingCas::default();

    let result = broker
        .fs_read(
            &FsRead::new(&path),
            &allow(EffectClass::FsRead),
            &mut cas,
            ResultBounds {
                max_preview_bytes: 9,
            },
        )
        .await
        .expect("bounded read");

    assert!(result.truncated);
    assert!(result.preview.len() <= 9);
    assert!(result.preview.is_char_boundary(result.preview.len()));
    assert!(result.artifact.is_some());
    assert_eq!(cas.writes, vec![full.into_bytes()]);
}

#[tokio::test]
async fn list_and_search_are_sorted_bounded_read_effects() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let root = directory.path();
    fs::create_dir(root.join("nested")).expect("create nested");
    fs::write(root.join("zeta.txt"), "needle z\n").expect("seed zeta");
    fs::write(root.join("alpha.txt"), "nothing\nneedle a\n").expect("seed alpha");
    fs::write(root.join("nested").join("beta.txt"), "needle b\n").expect("seed beta");
    let policy = allow(EffectClass::FsRead);
    let mut cas = RecordingCas::default();
    let mut broker = EffectBroker::new(RecordingJournal::default());

    let listed = broker
        .fs_list(
            &FsList::new(root),
            &policy,
            &mut cas,
            ResultBounds::default(),
        )
        .await
        .expect("list");
    assert_eq!(listed.preview, "alpha.txt\nnested/\nzeta.txt\n");

    let searched = broker
        .fs_search(
            &FsSearch::new(root, "needle"),
            &policy,
            &mut cas,
            ResultBounds::default(),
        )
        .await
        .expect("search");
    assert_eq!(
        searched.preview,
        "alpha.txt:2:needle a\nnested/beta.txt:1:needle b\nzeta.txt:1:needle z\n"
    );
    assert!(cas.writes.is_empty());
}
