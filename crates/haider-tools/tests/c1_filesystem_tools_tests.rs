#![cfg(unix)]
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::effect::{EffectClass, EffectPhase, FileFreshness};
use haider_protocol::ids::{ArtifactRef, RunId, SessionId};
use haider_tools::{
    CasSink, ChangeLedger, ChangeLedgerSink, EffectBroker, FsCaseMode, FsEdit, FsEditChange,
    FsGlob, FsPath, FsPathOperation, FsRead, FsSearch, FsSearchMode, FsWrite, FsWriteRecord,
    JournalSink, PermissionPolicy, ResultBounds, ToolError, ToolResult, TurnAttribution,
};
use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct JournalObserver(Arc<Mutex<Vec<EventPayload>>>);

impl JournalObserver {
    fn freshness(&self) -> Vec<FileFreshness> {
        self.0
            .lock()
            .expect("journal observer")
            .iter()
            .filter_map(|payload| match payload {
                EventPayload::Effect(EffectPhase::Outcome {
                    freshness: Some(record),
                    ..
                }) => Some(record.clone()),
                _ => None,
            })
            .collect()
    }
}

struct RecordingJournal(JournalObserver);

#[async_trait::async_trait]
impl JournalSink for RecordingJournal {
    async fn append(&mut self, payload: EventPayload) -> ToolResult<()> {
        self.0
            .0
            .lock()
            .map_err(|_| ToolError::journal("recording journal poisoned"))?
            .push(payload);
        Ok(())
    }
}

struct FailFirstOutcomeJournal {
    failed: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl JournalSink for FailFirstOutcomeJournal {
    async fn append(&mut self, payload: EventPayload) -> ToolResult<()> {
        if matches!(payload, EventPayload::Effect(EffectPhase::Outcome { .. }))
            && !self.failed.swap(true, Ordering::SeqCst)
        {
            return Err(ToolError::journal("injected terminal failure"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct RejectLedger;

impl ChangeLedgerSink for RejectLedger {
    fn record_fs_write(
        &self,
        _session: SessionId,
        _turn: RunId,
        _record: FsWriteRecord,
    ) -> ToolResult<()> {
        Err(ToolError::ledger("injected ledger failure"))
    }
}

#[derive(Default)]
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

    async fn put_file(&mut self, path: &Path) -> ToolResult<ArtifactRef> {
        let bytes =
            fs::read(path).map_err(|error| ToolError::cas(format!("read CAS input: {error}")))?;
        self.put(&bytes).await
    }
}

fn broker(root: &Path, session: &str, generation: u64) -> (EffectBroker, JournalObserver) {
    let observer = JournalObserver::default();
    let broker = EffectBroker::new_at(
        Box::new(RecordingJournal(observer.clone())),
        root,
        SessionId::new(session),
        generation,
        1_800_000_000_000 + generation,
    )
    .expect("broker");
    (broker, observer)
}

fn allow(class: EffectClass) -> PermissionPolicy {
    let mut policy = PermissionPolicy::default();
    policy.allow(class);
    policy
}

fn attribution(session: &str, turn: &str) -> TurnAttribution {
    TurnAttribution::new(SessionId::new(session), RunId::new(turn))
}

async fn read_file(broker: &mut EffectBroker, path: &str) -> String {
    broker
        .fs_read(
            &FsRead::new(path),
            &allow(EffectClass::FsRead),
            &mut RecordingCas::default(),
            ResultBounds::default(),
        )
        .await
        .expect("read file")
        .preview
}

#[tokio::test]
async fn file_read_range_is_line_numbered_bounded_and_tracks_the_full_digest() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let contents = "one\ntwo\nthree\n";
    fs::write(directory.path().join("lines.txt"), contents).expect("seed lines");
    let (mut broker, observer) = broker(directory.path(), "read-range", 1);

    let result = broker
        .fs_read(
            &FsRead::new("lines.txt").with_line_range(Some(2), Some(1)),
            &allow(EffectClass::FsRead),
            &mut RecordingCas::default(),
            ResultBounds::default(),
        )
        .await
        .expect("ranged read");

    assert_eq!(result.preview, "2: two\n");
    assert_eq!(
        observer.freshness(),
        vec![FileFreshness {
            path: "lines.txt".to_owned(),
            digest: format!("blake3:{}", blake3::hash(contents.as_bytes()).to_hex()),
        }]
    );
}

/// MUTATION CHECK: follow a descendant symlink or remove either result cap.
/// Expected RUNTIME failure: escaped content appears, search loses its full
/// CAS object/200-line preview, or glob returns more than 500 entries without
/// its truncation flag.
#[tokio::test]
async fn search_and_glob_are_root_confined_sorted_and_bounded() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let workspace = directory.path().join("workspace");
    let outside = directory.path().join("outside");
    fs::create_dir_all(workspace.join("src/nested")).expect("workspace tree");
    fs::create_dir(&outside).expect("outside");
    fs::write(outside.join("secret.rs"), "NEEDLE outside\n").expect("outside file");
    symlink(&outside, workspace.join("src/escape")).expect("escape symlink");
    fs::write(workspace.join("src/a.rs"), "needle one\nNEEDLE two\n").expect("a");
    fs::write(workspace.join("src/nested/b.rs"), "Needle three\n").expect("b");
    fs::write(workspace.join("src/skip.txt"), "NEEDLE skipped\n").expect("skip");
    let (mut search_broker, _) = broker(&workspace, "search", 1);
    let mut cas = RecordingCas::default();

    let search = search_broker
        .fs_search(
            &FsSearch::new("src", "needle")
                .with_glob("**/*.rs")
                .with_case_mode(FsCaseMode::Insensitive),
            &allow(EffectClass::FsRead),
            &mut cas,
            ResultBounds::default(),
        )
        .await
        .expect("search");
    assert_eq!(
        search.preview,
        "src/a.rs:1:needle one\nsrc/a.rs:2:NEEDLE two\nsrc/nested/b.rs:1:Needle three\n"
    );
    assert!(!search.preview.contains("outside"));

    let nested_glob = search_broker
        .fs_glob(
            &FsGlob::new("src", "**/*.rs"),
            &allow(EffectClass::FsRead),
            &mut cas,
            ResultBounds::default(),
        )
        .await
        .expect("nested glob");
    assert_eq!(nested_glob.preview, "src/a.rs\nsrc/nested/b.rs\n");

    for index in 0..501 {
        fs::write(workspace.join(format!("entry-{index:03}.rs")), "x").expect("glob entry");
    }
    let glob = search_broker
        .fs_glob(
            &FsGlob::new(".", "*.rs"),
            &allow(EffectClass::FsRead),
            &mut cas,
            ResultBounds {
                max_preview_bytes: 64 * 1024,
            },
        )
        .await
        .expect("glob");
    assert!(glob.truncated);
    assert_eq!(glob.preview.lines().count(), 500);
    assert_eq!(glob.preview.lines().next(), Some("entry-000.rs"));
    assert_eq!(glob.preview.lines().last(), Some("entry-499.rs"));

    let (mut refused, observer) = broker(&workspace, "refused", 2);
    let error = refused
        .fs_glob(
            &FsGlob::new("src/escape", "**"),
            &allow(EffectClass::FsRead),
            &mut RecordingCas::default(),
            ResultBounds::default(),
        )
        .await
        .expect_err("outside symlink must be refused");
    assert!(matches!(error, ToolError::WorkspaceBoundary { .. }));
    assert!(observer.0.lock().expect("journal").is_empty());
}

/// MUTATION CHECK: change the match cap to 201, omit the artifact, or use the
/// capped preview as CAS input. Expected RUNTIME failure: the literal 200/201
/// assertions or the last full-result line fails.
#[tokio::test]
async fn search_caps_preview_at_200_and_cas_preserves_every_match() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let mut contents = String::new();
    for index in 0..201 {
        contents.push_str(&format!("hit-{index:03}\n"));
    }
    fs::write(directory.path().join("hits.txt"), contents).expect("hits");
    let (mut broker, _) = broker(directory.path(), "search-cap", 1);
    let mut cas = RecordingCas::default();

    let result = broker
        .fs_search(
            &FsSearch::new(".", "hit-"),
            &allow(EffectClass::FsRead),
            &mut cas,
            ResultBounds {
                max_preview_bytes: 64 * 1024,
            },
        )
        .await
        .expect("bounded search");
    assert!(result.truncated);
    assert_eq!(result.preview.lines().count(), 200);
    assert!(result.artifact.is_some());
    let full = String::from_utf8(cas.writes.pop().expect("full CAS payload")).expect("UTF-8 CAS");
    assert_eq!(full.lines().count(), 201);
    assert!(full.ends_with("hits.txt:201:hit-200\n"));
}

/// MUTATION CHECK: apply only the match-count bound and omit the byte bound,
/// or cut a UTF-8 scalar. Expected RUNTIME failure: preview exceeds 8192,
/// decoding fails, or the complete long match is absent from CAS.
#[tokio::test]
async fn search_preview_is_eight_kib_utf8_safe_with_full_cas_overflow() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let line = format!("needle {}", "é".repeat(6_000));
    fs::write(directory.path().join("long.txt"), format!("{line}\n")).expect("long file");
    let (mut broker, _) = broker(directory.path(), "search-bytes", 1);
    let mut cas = RecordingCas::default();

    let result = broker
        .fs_search(
            &FsSearch::new(".", "needle"),
            &allow(EffectClass::FsRead),
            &mut cas,
            ResultBounds::default(),
        )
        .await
        .expect("long search");
    assert!(result.truncated);
    assert!(result.preview.len() <= 8 * 1024);
    assert!(std::str::from_utf8(result.preview.as_bytes()).is_ok());
    assert!(result.artifact.is_some());
    let full = String::from_utf8(cas.writes.pop().expect("full CAS payload")).expect("UTF-8 CAS");
    assert_eq!(full, format!("long.txt:1:{line}\n"));
}

/// MUTATION CHECK: treat smart case as always-insensitive, ignore the glob,
/// or downgrade simple matching to literal matching. Expected RUNTIME failure:
/// one of the literal result sets differs.
#[tokio::test]
async fn search_modes_case_and_glob_filters_are_effective() {
    let directory = tempfile::tempdir().expect("temporary directory");
    fs::write(directory.path().join("one.rs"), "Alpha beta\nalpine beta\n").expect("rs");
    fs::write(directory.path().join("two.txt"), "Alpha beta\n").expect("txt");
    let (mut broker, _) = broker(directory.path(), "search-modes", 1);
    let policy = allow(EffectClass::FsRead);

    let smart = broker
        .fs_search(
            &FsSearch::new(".", "Alpha")
                .with_case_mode(FsCaseMode::Smart)
                .with_glob("*.rs"),
            &policy,
            &mut RecordingCas::default(),
            ResultBounds::default(),
        )
        .await
        .expect("smart search");
    assert_eq!(smart.preview, "one.rs:1:Alpha beta\n");

    let simple = broker
        .fs_search(
            &FsSearch::new(".", "alp?ne")
                .with_case_mode(FsCaseMode::Insensitive)
                .with_mode(FsSearchMode::Simple)
                .with_glob("*.rs"),
            &policy,
            &mut RecordingCas::default(),
            ResultBounds::default(),
        )
        .await
        .expect("simple search");
    assert_eq!(simple.preview, "one.rs:2:alpine beta\n");
}

/// MUTATION CHECK: accept zero/multiple anchors for a singular edit or let
/// replace_all silently accept zero. Expected RUNTIME failure: the literal
/// match counts or unchanged bytes differ.
#[tokio::test]
async fn edit_requires_exactly_one_anchor_or_nonempty_replace_all() {
    let directory = tempfile::tempdir().expect("temporary directory");
    fs::write(directory.path().join("edit.txt"), "same same\n").expect("seed");
    let (mut broker, _) = broker(directory.path(), "anchors", 1);
    let policy = allow(EffectClass::FsWrite);
    let ledger = ChangeLedger::new();
    let turn = attribution("anchors", "turn");
    assert_eq!(read_file(&mut broker, "edit.txt").await, "same same\n");

    let missing = broker
        .fs_edit(
            &FsEdit::new("edit.txt", "absent", "x"),
            &policy,
            &turn,
            &ledger,
        )
        .await
        .expect_err("missing anchor");
    assert!(matches!(
        missing,
        ToolError::EditAnchor(ref mismatch) if mismatch.matches == 0
    ));

    let ambiguous = broker
        .fs_edit(
            &FsEdit::new("edit.txt", "same", "changed"),
            &policy,
            &turn,
            &ledger,
        )
        .await
        .expect_err("ambiguous anchor");
    assert!(matches!(
        ambiguous,
        ToolError::EditAnchor(ref mismatch) if mismatch.matches == 2
    ));
    assert_eq!(
        fs::read_to_string(directory.path().join("edit.txt")).expect("read"),
        "same same\n"
    );

    broker
        .fs_edit(
            &FsEdit::new("edit.txt", "same", "changed").replace_all(true),
            &policy,
            &turn,
            &ledger,
        )
        .await
        .expect("replace all");
    assert_eq!(
        fs::read_to_string(directory.path().join("edit.txt")).expect("read"),
        "changed changed\n"
    );
}

#[tokio::test]
async fn multi_edit_rejects_atomically_when_a_later_anchor_is_bad() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("batch.txt");
    fs::write(&path, "alpha beta gamma\n").expect("seed batch");
    let (mut broker, _) = broker(directory.path(), "batch", 1);
    assert_eq!(
        read_file(&mut broker, "batch.txt").await,
        "alpha beta gamma\n"
    );

    let error = broker
        .fs_edit(
            &FsEdit::many(
                "batch.txt",
                vec![
                    FsEditChange::new("alpha", "one"),
                    FsEditChange::new("missing", "two"),
                ],
            ),
            &allow(EffectClass::FsWrite),
            &attribution("batch", "turn"),
            &ChangeLedger::new(),
        )
        .await
        .expect_err("bad later anchor rejects batch");

    assert!(matches!(
        error,
        ToolError::EditAnchor(ref mismatch) if mismatch.matches == 0
    ));
    assert_eq!(
        fs::read_to_string(path).expect("read untouched batch"),
        "alpha beta gamma\n"
    );

    broker
        .fs_edit(
            &FsEdit::many(
                "batch.txt",
                vec![
                    FsEditChange::new("alpha", "one"),
                    FsEditChange::new("one beta", "two"),
                ],
            ),
            &allow(EffectClass::FsWrite),
            &attribution("batch", "turn"),
            &ChangeLedger::new(),
        )
        .await
        .expect("later edit sees earlier replacement");
    assert_eq!(
        fs::read_to_string(directory.path().join("batch.txt")).expect("read ordered batch"),
        "two gamma\n"
    );
}

#[tokio::test]
async fn write_creates_all_missing_parent_directories() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let (mut broker, _) = broker(directory.path(), "mkdir", 1);
    broker
        .fs_write(
            &FsWrite::new("one/two/three.txt", "nested\n"),
            &allow(EffectClass::FsWrite),
            &attribution("mkdir", "turn"),
            &ChangeLedger::new(),
        )
        .await
        .expect("nested write");
    assert_eq!(
        fs::read_to_string(directory.path().join("one/two/three.txt")).expect("read nested"),
        "nested\n"
    );
}

#[tokio::test]
async fn path_copy_move_and_delete_are_workspace_scoped_mutations() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let workspace = directory.path().join("workspace");
    let outside = directory.path().join("outside");
    fs::create_dir_all(workspace.join("source")).expect("source directory");
    fs::write(workspace.join("source/item.txt"), "payload").expect("source file");
    let (mut broker, _) = broker(&workspace, "paths", 1);
    let policy = allow(EffectClass::FsWrite);
    let turn = attribution("paths", "turn");
    let ledger = ChangeLedger::new();

    broker
        .fs_path(
            &FsPath::new(FsPathOperation::Copy, "source").with_destination("copied"),
            &policy,
            &turn,
            &ledger,
        )
        .await
        .expect("copy directory");
    assert_eq!(
        fs::read_to_string(workspace.join("copied/item.txt")).expect("copied file"),
        "payload"
    );

    broker
        .fs_path(
            &FsPath::new(FsPathOperation::Move, "copied/item.txt").with_destination("moved.txt"),
            &policy,
            &turn,
            &ledger,
        )
        .await
        .expect("move file");
    assert!(!workspace.join("copied/item.txt").exists());
    assert_eq!(
        fs::read_to_string(workspace.join("moved.txt")).expect("moved file"),
        "payload"
    );

    broker
        .fs_path(
            &FsPath::new(FsPathOperation::Delete, "copied"),
            &policy,
            &turn,
            &ledger,
        )
        .await
        .expect("delete directory");
    assert!(!workspace.join("copied").exists());

    let escape = broker
        .fs_path(
            &FsPath::new(FsPathOperation::Copy, "source").with_destination(&outside),
            &policy,
            &turn,
            &ledger,
        )
        .await
        .expect_err("outside destination rejected");
    assert!(matches!(escape, ToolError::WorkspaceBoundary { .. }));
    assert!(!outside.exists());
}

#[tokio::test]
async fn path_copy_overwrite_is_staged_and_destination_guards_are_typed() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let workspace = directory.path();
    fs::write(workspace.join("source.txt"), "new payload").expect("source");
    fs::write(workspace.join("destination.txt"), "old payload").expect("destination");
    symlink("source.txt", workspace.join("source-link")).expect("source symlink");
    let (mut broker, _) = broker(workspace, "path-overwrite", 1);
    let policy = allow(EffectClass::FsWrite);
    let turn = attribution("path-overwrite", "turn");
    let ledger = ChangeLedger::new();

    let exists = broker
        .fs_path(
            &FsPath::new(FsPathOperation::Copy, "source.txt").with_destination("destination.txt"),
            &policy,
            &turn,
            &ledger,
        )
        .await
        .expect_err("existing destination requires overwrite");
    assert!(matches!(exists, ToolError::InvalidArgument { .. }));
    assert_eq!(
        fs::read_to_string(workspace.join("destination.txt")).expect("unchanged destination"),
        "old payload"
    );

    let refused_source = broker
        .fs_path(
            &FsPath::new(FsPathOperation::Copy, "source-link")
                .with_destination("destination.txt")
                .overwrite(true),
            &policy,
            &turn,
            &ledger,
        )
        .await
        .expect_err("copying a source symlink is refused");
    assert!(matches!(refused_source, ToolError::PathChanged { .. }));
    assert_eq!(
        fs::read_to_string(workspace.join("destination.txt")).expect("preserved destination"),
        "old payload"
    );

    broker
        .fs_path(
            &FsPath::new(FsPathOperation::Copy, "source.txt")
                .with_destination("destination.txt")
                .overwrite(true),
            &policy,
            &turn,
            &ledger,
        )
        .await
        .expect("staged overwrite");
    assert_eq!(
        fs::read_to_string(workspace.join("destination.txt")).expect("overwritten destination"),
        "new payload"
    );
    assert!(
        fs::read_dir(workspace)
            .expect("workspace entries")
            .all(|entry| !entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .starts_with(".haider-path-"))
    );

    fs::hard_link(
        workspace.join("source.txt"),
        workspace.join("source-alias.txt"),
    )
    .expect("source hardlink alias");
    let alias_move = broker
        .fs_path(
            &FsPath::new(FsPathOperation::Move, "source.txt")
                .with_destination("source-alias.txt")
                .overwrite(true),
            &policy,
            &turn,
            &ledger,
        )
        .await
        .expect_err("moving between hardlink aliases is refused");
    assert!(matches!(alias_move, ToolError::InvalidArgument { .. }));
    assert!(workspace.join("source.txt").exists());
    assert!(workspace.join("source-alias.txt").exists());

    let root_delete = broker
        .fs_path(
            &FsPath::new(FsPathOperation::Delete, "."),
            &policy,
            &turn,
            &ledger,
        )
        .await
        .expect_err("workspace root delete is refused");
    assert!(matches!(root_delete, ToolError::InvalidArgument { .. }));
}

/// MUTATION CHECK: use lossy UTF-8 conversion in fs_edit. Expected RUNTIME
/// failure: the typed invalid-argument assertion or exact binary bytes differ.
#[tokio::test]
async fn edit_refuses_non_utf8_without_mutating_bytes() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let bytes = [b'a', 0xff, b'b'];
    fs::write(directory.path().join("binary.txt"), bytes).expect("seed binary");
    let (mut broker, _) = broker(directory.path(), "binary", 1);
    broker
        .restore_freshness([FileFreshness {
            path: "binary.txt".into(),
            digest: format!("blake3:{}", blake3::hash(&bytes).to_hex()),
        }])
        .expect("restore exact binary digest");

    let error = broker
        .fs_edit(
            &FsEdit::new("binary.txt", "a", "z"),
            &allow(EffectClass::FsWrite),
            &attribution("binary", "turn"),
            &ChangeLedger::new(),
        )
        .await
        .expect_err("binary edit refused");
    assert!(matches!(error, ToolError::InvalidArgument { .. }));
    assert_eq!(
        fs::read(directory.path().join("binary.txt")).expect("read binary"),
        bytes
    );
}

/// MUTATION CHECK: allow an absent digest for an existing target. Expected
/// RUNTIME failure: either mutation succeeds instead of returning the two
/// literal `UnreadFile` variants and the seeded bytes change.
#[tokio::test]
async fn unread_existing_edit_and_write_are_typed_refusals() {
    let directory = tempfile::tempdir().expect("temporary directory");
    fs::write(directory.path().join("unread.txt"), "original\n").expect("seed");
    let (mut broker, _) = broker(directory.path(), "unread", 1);
    let policy = allow(EffectClass::FsWrite);
    let ledger = ChangeLedger::new();
    let turn = attribution("unread", "turn");

    let edit = broker
        .fs_edit(
            &FsEdit::new("unread.txt", "original", "edited"),
            &policy,
            &turn,
            &ledger,
        )
        .await
        .expect_err("unread edit");
    assert!(matches!(edit, ToolError::UnreadFile { .. }));
    let write = broker
        .fs_write(
            &FsWrite::new("unread.txt", "written\n"),
            &policy,
            &turn,
            &ledger,
        )
        .await
        .expect_err("unread overwrite");
    assert!(matches!(write, ToolError::UnreadFile { .. }));
    assert_eq!(
        fs::read_to_string(directory.path().join("unread.txt")).expect("read"),
        "original\n"
    );
}

/// MUTATION CHECK: compare against a production-derived/shared digest or omit
/// the locked current-byte comparison. Expected RUNTIME failure: the external
/// literal survives neither the typed stale verdict nor the filesystem check.
#[tokio::test]
async fn stale_mutation_is_typed_and_requires_a_reread() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("stale.txt");
    fs::write(&path, "before\n").expect("seed");
    let (mut broker, _) = broker(directory.path(), "stale", 1);
    let ledger = ChangeLedger::new();
    let turn = attribution("stale", "turn");
    assert_eq!(read_file(&mut broker, "stale.txt").await, "before\n");
    fs::write(&path, "external literal\n").expect("external edit");

    let error = broker
        .fs_edit(
            &FsEdit::new("stale.txt", "before", "after"),
            &allow(EffectClass::FsWrite),
            &turn,
            &ledger,
        )
        .await
        .expect_err("stale edit");
    let ToolError::StaleRead {
        recorded_digest,
        current_digest,
        ..
    } = error
    else {
        panic!("expected stale_read");
    };
    assert_ne!(recorded_digest, current_digest);
    assert_eq!(
        fs::read_to_string(&path).expect("read external"),
        "external literal\n"
    );

    assert_eq!(
        read_file(&mut broker, "stale.txt").await,
        "external literal\n"
    );
    broker
        .fs_edit(
            &FsEdit::new("stale.txt", "external literal", "after reread"),
            &allow(EffectClass::FsWrite),
            &turn,
            &ledger,
        )
        .await
        .expect("fresh edit");

    // The WRITE-over-existing gate is a SECOND enforcement point: an
    // external change after the fresh edit must trip fs_write too, not
    // only fs_edit (each gate needs its own observation — disabling one
    // must not hide behind the other).
    fs::write(
        &path,
        "external literal two
",
    )
    .expect("external write two");
    let stale_write = broker
        .fs_write(
            &FsWrite::new(
                "stale.txt",
                "overwrite attempt
",
            ),
            &allow(EffectClass::FsWrite),
            &turn,
            &ledger,
        )
        .await;
    assert!(
        matches!(stale_write, Err(ToolError::StaleRead { .. })),
        "stale write must be typed StaleRead, got {stale_write:?}"
    );
    assert_eq!(
        fs::read_to_string(&path).expect("read after refused write"),
        "external literal two\n"
    );
}

/// MUTATION CHECK: omit successful write/edit freshness from the terminal
/// outcome. Expected RUNTIME failure: the second or third mutation returns
/// stale/unread instead of completing the literal chain.
#[tokio::test]
async fn self_edit_and_write_chains_never_retrip_freshness() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let (mut broker, _) = broker(directory.path(), "self-chain", 1);
    let ledger = ChangeLedger::new();
    let turn = attribution("self-chain", "turn");
    let policy = allow(EffectClass::FsWrite);

    broker
        .fs_write(&FsWrite::new("chain.txt", "one"), &policy, &turn, &ledger)
        .await
        .expect("create seeds freshness");
    broker
        .fs_write(&FsWrite::new("chain.txt", "two"), &policy, &turn, &ledger)
        .await
        .expect("self write remains fresh");
    broker
        .fs_edit(
            &FsEdit::new("chain.txt", "two", "three"),
            &policy,
            &turn,
            &ledger,
        )
        .await
        .expect("self edit remains fresh");
    broker
        .fs_edit(
            &FsEdit::new("chain.txt", "three", "four"),
            &policy,
            &turn,
            &ledger,
        )
        .await
        .expect("second self edit remains fresh");
    assert_eq!(
        fs::read_to_string(directory.path().join("chain.txt")).expect("read"),
        "four"
    );
}

/// MUTATION CHECK: do not restore Outcome freshness, or restore from a
/// nonterminal phase. Expected RUNTIME failure: unchanged restart edit is
/// unread, or the externally changed restart is not stale.
#[tokio::test]
async fn recovery_rebuilds_both_fresh_and_stale_verdicts_from_outcomes() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("restart.txt");
    fs::write(&path, "first\n").expect("seed");
    let (mut first, first_journal) = broker(directory.path(), "restart", 1);
    assert_eq!(read_file(&mut first, "restart.txt").await, "first\n");
    let records = first_journal.freshness();
    assert_eq!(records.len(), 1);

    let (mut unchanged, _) = broker(directory.path(), "restart", 2);
    unchanged
        .restore_freshness(records.clone())
        .expect("restore freshness");
    unchanged
        .fs_edit(
            &FsEdit::new("restart.txt", "first", "second"),
            &allow(EffectClass::FsWrite),
            &attribution("restart", "turn-2"),
            &ChangeLedger::new(),
        )
        .await
        .expect("unchanged restart stays fresh");

    let (mut stale, _) = broker(directory.path(), "restart", 3);
    stale
        .restore_freshness(records)
        .expect("restore old freshness");
    let error = stale
        .fs_edit(
            &FsEdit::new("restart.txt", "first", "third"),
            &allow(EffectClass::FsWrite),
            &attribution("restart", "turn-3"),
            &ChangeLedger::new(),
        )
        .await
        .expect_err("old restart state must be stale");
    assert!(matches!(error, ToolError::StaleRead { .. }));
}

/// MUTATION CHECK: share freshness globally across sessions or refresh the
/// parent's map from the child's outcome. Expected RUNTIME failure: the
/// parent's final mutation does not return stale_read after the child edit.
#[tokio::test]
async fn child_edit_trips_parent_stale_without_sharing_session_state() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("shared.txt");
    fs::write(&path, "parent saw this\n").expect("seed");
    let (mut parent, parent_journal) = broker(directory.path(), "parent", 1);
    assert_eq!(
        read_file(&mut parent, "shared.txt").await,
        "parent saw this\n"
    );

    let (mut child, _) = broker(directory.path(), "child", 1);
    assert_eq!(
        read_file(&mut child, "shared.txt").await,
        "parent saw this\n"
    );
    child
        .fs_edit(
            &FsEdit::new("shared.txt", "parent saw this", "child changed it"),
            &allow(EffectClass::FsWrite),
            &attribution("child", "child-turn"),
            &ChangeLedger::new(),
        )
        .await
        .expect("child edit");

    let (mut resumed_parent, _) = broker(directory.path(), "parent", 2);
    resumed_parent
        .restore_freshness(parent_journal.freshness())
        .expect("restore parent freshness only");
    let error = resumed_parent
        .fs_edit(
            &FsEdit::new("shared.txt", "parent saw this", "parent edit"),
            &allow(EffectClass::FsWrite),
            &attribution("parent", "parent-turn"),
            &ChangeLedger::new(),
        )
        .await
        .expect_err("child must stale parent");
    assert!(matches!(error, ToolError::StaleRead { .. }));
    assert_eq!(
        fs::read_to_string(path).expect("read child bytes"),
        "child changed it\n"
    );
}

/// MUTATION CHECK: advance in-memory freshness before the terminal append.
/// Expected RUNTIME failure: the edit proceeds (or becomes stale) instead of
/// the literal unread_file verdict after the injected append failure.
#[tokio::test]
async fn failed_terminal_append_never_advances_freshness() {
    let directory = tempfile::tempdir().expect("temporary directory");
    fs::write(directory.path().join("terminal.txt"), "before\n").expect("seed");
    let mut broker = EffectBroker::new_at(
        Box::new(FailFirstOutcomeJournal {
            failed: Arc::new(AtomicBool::new(false)),
        }),
        directory.path(),
        SessionId::new("terminal-failure"),
        1,
        1_800_000_000_100,
    )
    .expect("broker");
    let read = broker
        .fs_read(
            &FsRead::new("terminal.txt"),
            &allow(EffectClass::FsRead),
            &mut RecordingCas::default(),
            ResultBounds::default(),
        )
        .await
        .expect_err("terminal append fails");
    assert!(matches!(read, ToolError::Journal { .. }));

    let edit = broker
        .fs_edit(
            &FsEdit::new("terminal.txt", "before", "after"),
            &allow(EffectClass::FsWrite),
            &attribution("terminal-failure", "turn"),
            &ChangeLedger::new(),
        )
        .await
        .expect_err("failed read outcome cannot make file fresh");
    assert!(matches!(edit, ToolError::UnreadFile { .. }));
}

/// MUTATION CHECK: discard landed bytes when the change ledger rejects its
/// evidence. Expected RUNTIME failure: the second write reports stale_read
/// instead of using the digest carried by the Failed terminal outcome.
#[tokio::test]
async fn landed_write_with_ledger_failure_still_updates_freshness() {
    let directory = tempfile::tempdir().expect("temporary directory");
    fs::write(directory.path().join("ledger.txt"), "one").expect("seed");
    let (mut broker, _) = broker(directory.path(), "ledger-freshness", 1);
    assert_eq!(read_file(&mut broker, "ledger.txt").await, "one");
    let turn = attribution("ledger-freshness", "turn");
    let policy = allow(EffectClass::FsWrite);

    let error = broker
        .fs_write(
            &FsWrite::new("ledger.txt", "two"),
            &policy,
            &turn,
            &RejectLedger,
        )
        .await
        .expect_err("ledger failure is surfaced");
    assert!(matches!(error, ToolError::Ledger { .. }));
    assert_eq!(
        fs::read_to_string(directory.path().join("ledger.txt")).expect("read"),
        "two"
    );

    broker
        .fs_write(
            &FsWrite::new("ledger.txt", "three"),
            &policy,
            &turn,
            &ChangeLedger::new(),
        )
        .await
        .expect("landed write digest remains fresh");
    assert_eq!(
        fs::read_to_string(directory.path().join("ledger.txt")).expect("read"),
        "three"
    );
}

/// MUTATION CHECK: alter the legacy result projection while adding freshness.
/// Expected RUNTIME failure: one of the exact old preview/flag/ref assertions
/// differs despite C1-only options being unused.
#[tokio::test]
async fn existing_read_and_create_write_results_remain_byte_exact() {
    let directory = tempfile::tempdir().expect("temporary directory");
    fs::write(directory.path().join("read.txt"), "exact\n").expect("seed");
    let (mut broker, _) = broker(directory.path(), "golden", 1);
    let read = broker
        .fs_read(
            &FsRead::new("read.txt"),
            &allow(EffectClass::FsRead),
            &mut RecordingCas::default(),
            ResultBounds::default(),
        )
        .await
        .expect("read");
    assert_eq!(read.preview, "exact\n");
    assert!(!read.truncated);
    assert!(read.artifact.is_none());
    assert!(read.cursor.is_none());

    let canonical = fs::canonicalize(directory.path())
        .expect("canonical root")
        .join("new.txt");
    let write = broker
        .fs_write(
            &FsWrite::new("new.txt", "bytes"),
            &allow(EffectClass::FsWrite),
            &attribution("golden", "turn"),
            &ChangeLedger::new(),
        )
        .await
        .expect("create");
    assert_eq!(
        write.preview,
        format!("wrote 5 bytes to {}", canonical.display())
    );
    assert!(!write.truncated);
    assert!(write.artifact.is_none());
    assert!(write.cursor.is_none());
}
