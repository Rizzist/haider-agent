#![allow(clippy::expect_used)]

use super::{
    WorkspaceReceiptCoverage, WorkspaceReceiptStrategy, WorkspaceReceiptTracker,
    WorkspaceReceiptUnknownReason, porcelain_v1_paths,
};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn test_deadline() -> Instant {
    // These fixtures have finite entry/content bounds. Wall-time behavior is
    // tested separately with an already-expired deadline; it must not pre-empt
    // an unrelated assertion when the machine is running other build lanes.
    // The budget stays bounded so a hung receipt fails rather than stalls.
    Instant::now() + super::TEST_RECEIPT_WALL_TIME
}

fn receipt_with_git_for_test(root: &Path, git_program: &OsStr) -> super::WorkspaceStateReceipt {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("receipt runtime")
        .block_on(super::compute_workspace_state_receipt_async_with_deadline(
            root.to_path_buf(),
            git_program.to_os_string(),
            test_deadline(),
        ))
}

fn receipt_for_test(root: &Path) -> super::WorkspaceStateReceipt {
    receipt_with_git_for_test(root, OsStr::new("git"))
}

#[cfg(unix)]
fn fake_git(fixture: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;

    let fake_git = fixture.join("fake-git");
    std::fs::write(
        &fake_git,
        "#!/bin/sh\ntouch \"$(dirname \"$0\")/git-invoked\"\nexit 99\n",
    )
    .expect("fake git");
    let mut permissions = std::fs::metadata(&fake_git)
        .expect("fake git metadata")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&fake_git, permissions).expect("fake git executable");
    fake_git
}

#[test]
fn non_repository_receipt_is_explicitly_unknown_without_enumeration() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::create_dir(workspace.path().join("wide")).expect("wide directory");
    for index in 0..32 {
        std::fs::write(
            workspace.path().join("wide").join(format!("entry-{index}")),
            b"content",
        )
        .expect("wide entry");
    }

    let receipt = receipt_for_test(workspace.path());

    assert_eq!(
        receipt.coverage,
        WorkspaceReceiptCoverage::Unknown(WorkspaceReceiptUnknownReason::NonRepository)
    );
    assert_eq!(receipt.entries_visited, 0);
    assert!(
        receipt
            .mutation_digest()
            .contains("reason=not_enumerated_non_repository")
    );
}

/// F2: the synchronous wrapper (worker thread, runtime and channel) must hand
/// back the computed receipt, not substitute its own wall-time `Unknown`.
/// Driven through the wrapper's internal entry point with the bounded test
/// budget so the assertion cannot race the production 500 ms timeout.
#[test]
fn sync_wrapper_returns_the_computed_non_repository_receipt() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("source.rs"), b"source").expect("source");

    let receipt = super::workspace_state_receipt_with_git(
        workspace.path().to_path_buf(),
        "git".into(),
        super::TEST_RECEIPT_WALL_TIME,
    );

    assert_eq!(
        receipt.coverage,
        WorkspaceReceiptCoverage::Unknown(WorkspaceReceiptUnknownReason::NonRepository)
    );
    assert_eq!(receipt.strategy, WorkspaceReceiptStrategy::NotEnumerated);
    assert!(
        receipt.counts_known,
        "a classified receipt keeps exact counts"
    );
    assert_eq!(receipt.entries_visited, 0);
    assert_eq!(receipt.content_bytes_read, 0);
    assert_eq!(receipt, receipt_for_test(workspace.path()));
}

/// F2: a complete repository receipt crosses the wrapper unchanged.
#[test]
fn sync_wrapper_returns_the_computed_complete_receipt() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::create_dir(workspace.path().join(".git")).expect("repository marker");
    std::fs::write(workspace.path().join("source.rs"), b"stable").expect("source");

    let receipt = super::workspace_state_receipt_with_git(
        workspace.path().to_path_buf(),
        "definitely-not-a-git-binary".into(),
        super::TEST_RECEIPT_WALL_TIME,
    );

    assert_eq!(receipt.coverage, WorkspaceReceiptCoverage::Complete);
    assert_eq!(receipt.strategy, WorkspaceReceiptStrategy::RepositoryWalk);
    assert_eq!(
        receipt,
        receipt_with_git_for_test(workspace.path(), OsStr::new("definitely-not-a-git-binary"))
    );
}

/// The public entry points keep the production budget, so on a loaded machine
/// they may legitimately report the wall limit; any other outcome is a defect.
#[test]
fn public_receipt_entry_points_classify_a_non_repository_root() {
    let workspace = tempfile::tempdir().expect("workspace");

    let receipt = super::workspace_state_receipt(workspace.path());
    let digest = super::workspace_state_digest(workspace.path());

    match receipt.coverage {
        WorkspaceReceiptCoverage::Unknown(WorkspaceReceiptUnknownReason::NonRepository) => {
            assert!(receipt.counts_known);
            assert_eq!(receipt.entries_visited, 0);
        }
        WorkspaceReceiptCoverage::Unknown(WorkspaceReceiptUnknownReason::WallTimeLimit) => {
            assert!(!receipt.counts_known);
        }
        other => panic!("unexpected public receipt coverage: {other:?}"),
    }
    assert!(
        digest.contains("reason=not_enumerated_non_repository")
            || digest.contains("reason=wall_time_limit"),
        "{digest}"
    );
}

#[test]
fn repository_receipt_does_not_export_unkeyed_secret_identity() {
    let fixture = tempfile::tempdir().expect("fixture");
    let workspace = fixture.path().join("workspace");
    std::fs::create_dir_all(workspace.join(".git")).expect("repository marker");
    std::fs::write(workspace.join("oracle.txt"), "password=violet-sunrise\n")
        .expect("synthetic credential");
    let receipt = receipt_for_test(&workspace);
    assert_eq!(
        receipt.coverage,
        WorkspaceReceiptCoverage::Unknown(WorkspaceReceiptUnknownReason::RedactedMaterial)
    );
    assert!(!receipt.counts_known);
    let published = receipt.mutation_digest();
    assert!(published.contains("reason=redacted_material"));
    assert!(!published.contains("violet-sunrise"));
    assert!(!published.contains("oracle.txt"));
    assert!(published.contains("bytes=unreported"));
    std::fs::write(workspace.join("oracle.txt"), "password=violet-morning\n")
        .expect("equal-length replacement");
    let next = receipt_for_test(&workspace);
    assert_eq!(
        published,
        next.mutation_digest(),
        "unknown receipt cannot distinguish equal-length masked values"
    );
}

#[test]
fn git_status_with_sensitive_path_has_no_guessable_receipt() {
    let workspace = tempfile::tempdir().expect("workspace");
    let initialized = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(workspace.path())
        .status()
        .expect("git init");
    assert!(initialized.success());
    std::fs::write(workspace.path().join(".env"), "ordinary before\n").expect("synthetic file");
    let added = std::process::Command::new("git")
        .args(["add", ".env"])
        .current_dir(workspace.path())
        .status()
        .expect("git add");
    assert!(added.success());
    std::fs::write(workspace.path().join(".env"), "ordinary after\n").expect("synthetic mutation");

    let receipt = receipt_for_test(workspace.path());
    assert_eq!(
        receipt.coverage,
        WorkspaceReceiptCoverage::Unknown(WorkspaceReceiptUnknownReason::RedactedMaterial)
    );
    assert!(!receipt.counts_known);
    assert!(receipt.mutation_digest().contains("bytes=unreported"));
}

#[cfg(unix)]
#[test]
fn entry_limit_prevents_git_from_being_invoked() {
    let fixture = tempfile::tempdir().expect("fixture");
    let workspace = fixture.path().join("workspace");
    std::fs::create_dir_all(workspace.join(".git")).expect("repository marker");
    std::fs::create_dir(workspace.join("wide")).expect("wide directory");
    for index in 0..4_100 {
        std::fs::write(workspace.join("wide").join(format!("entry-{index}")), b"")
            .expect("wide entry");
    }
    let fake_git = fake_git(fixture.path());

    let receipt = receipt_with_git_for_test(&workspace, fake_git.as_os_str());

    assert_eq!(
        receipt.coverage,
        WorkspaceReceiptCoverage::Unknown(WorkspaceReceiptUnknownReason::EntryLimit)
    );
    assert_eq!(receipt.entries_visited, 4_097);
    assert!(!fixture.path().join("git-invoked").exists());
}

#[cfg(unix)]
#[test]
fn linked_worktree_metadata_outside_the_workspace_uses_the_anchored_fallback() {
    let fixture = tempfile::tempdir().expect("fixture");
    let workspace = fixture.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    std::fs::write(workspace.join(".git"), "gitdir: ../outside-metadata\n")
        .expect("worktree marker");
    std::fs::write(workspace.join("source.rs"), "before").expect("source");
    let fake_git = fake_git(fixture.path());

    let before = receipt_with_git_for_test(&workspace, fake_git.as_os_str());
    std::fs::write(workspace.join("source.rs"), "after").expect("mutate source");
    let after = receipt_with_git_for_test(&workspace, fake_git.as_os_str());

    assert_eq!(before.strategy, WorkspaceReceiptStrategy::RepositoryWalk);
    assert_eq!(before.coverage, WorkspaceReceiptCoverage::Complete);
    assert_ne!(before.mutation_digest(), after.mutation_digest());
    assert!(!fixture.path().join("git-invoked").exists());
}

#[test]
fn porcelain_parser_keeps_both_rename_paths_and_untracked_paths() {
    let paths = porcelain_v1_paths(b"R  new.rs\0old.rs\0?? loose.rs\0").expect("porcelain paths");
    assert_eq!(
        paths,
        vec![
            PathBuf::from("new.rs"),
            PathBuf::from("old.rs"),
            PathBuf::from("loose.rs"),
        ]
    );
}

#[test]
fn unavailable_git_falls_back_without_failing_the_receipt() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::create_dir(workspace.path().join(".git")).expect("repository marker");
    // A readable index header makes preparation actually attempt Git. An
    // empty .git directory would bypass Git before testing the fallback.
    std::fs::write(
        workspace.path().join(".git/index"),
        b"DIRC\0\0\0\x02\0\0\0\0",
    )
    .expect("synthetic empty index header");
    std::fs::write(workspace.path().join("source.rs"), b"before").expect("source");

    let before =
        receipt_with_git_for_test(workspace.path(), OsStr::new("definitely-not-a-git-binary"));
    std::fs::write(workspace.path().join("source.rs"), b"after").expect("mutate source");
    let after =
        receipt_with_git_for_test(workspace.path(), OsStr::new("definitely-not-a-git-binary"));

    assert_eq!(before.strategy, WorkspaceReceiptStrategy::RepositoryWalk);
    assert_eq!(before.coverage, WorkspaceReceiptCoverage::Complete);
    assert_eq!(after.coverage, WorkspaceReceiptCoverage::Complete);
    assert_ne!(before.mutation_digest(), after.mutation_digest());
}

#[test]
fn available_git_overlays_porcelain_on_the_bounded_anchored_walk() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("source.rs"), b"source").expect("source");
    let initialized = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(workspace.path())
        .status()
        .expect("git init");
    assert!(initialized.success());
    let added = std::process::Command::new("git")
        .args(["add", "source.rs"])
        .current_dir(workspace.path())
        .status()
        .expect("git add");
    assert!(added.success());

    let receipt = receipt_for_test(workspace.path());

    assert_eq!(receipt.strategy, WorkspaceReceiptStrategy::GitStatus);
    assert_eq!(receipt.coverage, WorkspaceReceiptCoverage::Complete);
}

#[test]
fn observed_length_content_budget_is_explicitly_elided() {
    const CONTENT_BUDGET: u64 = 64;
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("oversized.bin"), [b'x'; 65]).expect("oversized file");
    let reader = super::AnchoredWorkspaceReader::new(workspace.path()).expect("anchored reader");
    let mut builder =
        super::ReceiptBuilder::new(WorkspaceReceiptStrategy::RepositoryWalk, test_deadline());
    assert_eq!(
        builder.content_budget,
        super::WORKSPACE_RECEIPT_CONTENT_BUDGET_BYTES
    );
    // Model a nearly exhausted aggregate budget rather than redacting 16 MiB
    // just to exercise the observed-length/content-elided branch.
    builder.content_budget = CONTENT_BUDGET;
    builder.content_bytes_read = super::WORKSPACE_RECEIPT_CONTENT_BUDGET_BYTES - CONTENT_BUDGET;

    let reason = builder
        .hash_path(&reader, Path::new("oversized.bin"))
        .expect_err("content cap must elide the observed suffix");
    builder.mark_unknown(reason);
    let receipt = builder.finish();

    assert_eq!(
        receipt.coverage,
        WorkspaceReceiptCoverage::Unknown(WorkspaceReceiptUnknownReason::ContentLimit)
    );
    assert_eq!(
        receipt.content_bytes_read,
        super::WORKSPACE_RECEIPT_CONTENT_BUDGET_BYTES
    );
    assert_eq!(receipt.entries_visited, 1);
    assert!(receipt.counts_known);
    assert!(receipt.mutation_digest().contains("reason=content_limit"));
}

#[test]
fn exact_content_budget_remains_complete() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("exact.bin"), [b'x'; 64]).expect("exact file");
    let reader = super::AnchoredWorkspaceReader::new(workspace.path()).expect("anchored reader");
    let mut builder =
        super::ReceiptBuilder::new(WorkspaceReceiptStrategy::RepositoryWalk, test_deadline());
    builder.content_budget = 64;
    builder
        .hash_path(&reader, Path::new("exact.bin"))
        .expect("exact budget");
    assert_eq!(builder.content_budget, 0);
    let receipt = builder.finish();
    assert_eq!(receipt.coverage, WorkspaceReceiptCoverage::Complete);
    assert_eq!(receipt.content_bytes_read, 64);
}

#[test]
fn depleted_content_budget_elides_the_next_nonempty_file() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("first.bin"), b"x").expect("first file");
    std::fs::write(workspace.path().join("next.bin"), b"x").expect("next file");
    let reader = super::AnchoredWorkspaceReader::new(workspace.path()).expect("anchored reader");
    let mut builder =
        super::ReceiptBuilder::new(WorkspaceReceiptStrategy::RepositoryWalk, test_deadline());
    builder.content_budget = 1;
    builder
        .hash_path(&reader, Path::new("first.bin"))
        .expect("first byte");
    let reason = builder
        .hash_path(&reader, Path::new("next.bin"))
        .expect_err("depleted budget");
    assert_eq!(reason, WorkspaceReceiptUnknownReason::ContentLimit);
    assert_eq!(builder.content_bytes_read, 1);
    assert_eq!(builder.entries_visited, 2);
}

#[test]
fn empty_file_remains_complete_with_zero_content_budget() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("empty.bin"), b"").expect("empty file");
    let reader = super::AnchoredWorkspaceReader::new(workspace.path()).expect("anchored reader");
    let mut builder =
        super::ReceiptBuilder::new(WorkspaceReceiptStrategy::RepositoryWalk, test_deadline());
    builder.content_budget = 0;
    builder
        .hash_path(&reader, Path::new("empty.bin"))
        .expect("empty content");
    let receipt = builder.finish();
    assert_eq!(receipt.coverage, WorkspaceReceiptCoverage::Complete);
    assert_eq!(receipt.content_bytes_read, 0);
}

#[test]
fn content_budget_across_read_chunks_is_explicitly_elided() {
    let workspace = tempfile::tempdir().expect("workspace");
    let mut content = vec![b'x'; 64 * 1024 + 2];
    content[32 * 1024 - 1] = b'\n';
    content[64 * 1024 - 1] = b'\n';
    std::fs::write(workspace.path().join("chunks.bin"), content).expect("chunked file");
    let reader = super::AnchoredWorkspaceReader::new(workspace.path()).expect("anchored reader");
    let mut builder =
        super::ReceiptBuilder::new(WorkspaceReceiptStrategy::RepositoryWalk, test_deadline());
    builder.content_budget = 64 * 1024 + 1;
    let reason = builder
        .hash_path(&reader, Path::new("chunks.bin"))
        .expect_err("suffix elided");
    assert_eq!(reason, WorkspaceReceiptUnknownReason::ContentLimit);
    assert_eq!(builder.content_bytes_read, 64 * 1024 + 1);
    assert_eq!(builder.content_budget, 0);
}

#[test]
fn expired_receipt_deadline_still_preempts_content_work() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::create_dir(workspace.path().join(".git")).expect("repository marker");
    std::fs::write(workspace.path().join("source.rs"), b"x").expect("source");
    let reader = super::AnchoredWorkspaceReader::new(workspace.path()).expect("anchored reader");
    let deadline = Instant::now();
    let mut builder =
        super::ReceiptBuilder::new(WorkspaceReceiptStrategy::RepositoryWalk, deadline);
    builder.content_budget = 0;
    assert_eq!(
        builder.hash_path(&reader, Path::new("source.rs")),
        Err(WorkspaceReceiptUnknownReason::WallTimeLimit)
    );
    assert_eq!(builder.content_bytes_read, 0);
    assert_eq!(builder.entries_visited, 0);
    let receipt = super::repository_walk_receipt(workspace.path(), deadline);
    assert_eq!(
        receipt.coverage,
        WorkspaceReceiptCoverage::Unknown(WorkspaceReceiptUnknownReason::WallTimeLimit)
    );
}

#[test]
fn production_receipt_limits_remain_pinned() {
    assert_eq!(
        super::WORKSPACE_RECEIPT_WALL_TIME,
        Duration::from_millis(500)
    );
    assert_eq!(
        super::WORKSPACE_RECEIPT_CONTENT_BUDGET_BYTES,
        16 * 1024 * 1024
    );
    assert_eq!(super::WORKSPACE_RECEIPT_MAX_ENTRIES, 4_096);
    assert_eq!(
        WorkspaceReceiptTracker::default().wall_time(),
        Duration::from_millis(500),
        "only the cfg(test) seam may change a tracker's receipt budget"
    );
    let test_budget = super::TEST_RECEIPT_WALL_TIME;
    assert!(
        (Duration::from_secs(60)..=Duration::from_secs(120)).contains(&test_budget),
        "the test budget must stay bounded so a hung receipt fails: {test_budget:?}"
    );
}

#[cfg(unix)]
#[test]
fn repository_fallback_does_not_follow_a_symlink_cycle() {
    use std::os::unix::fs::symlink;

    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::create_dir(workspace.path().join(".git")).expect("repository marker");
    std::fs::create_dir(workspace.path().join("nested")).expect("nested");
    std::fs::write(workspace.path().join("nested/source.rs"), b"source").expect("source");
    symlink("..", workspace.path().join("nested/cycle")).expect("cycle");

    let receipt =
        receipt_with_git_for_test(workspace.path(), OsStr::new("definitely-not-a-git-binary"));

    assert_eq!(receipt.strategy, WorkspaceReceiptStrategy::RepositoryWalk);
    assert_eq!(
        receipt.coverage,
        WorkspaceReceiptCoverage::Unknown(WorkspaceReceiptUnknownReason::SymlinkOrReparsePoint)
    );
}

#[test]
fn an_interleaved_known_filesystem_mutation_invalidates_a_live_process_comparison() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::create_dir(workspace.path().join(".git")).expect("repository marker");
    std::fs::write(workspace.path().join("source.rs"), b"stable").expect("source");
    let receipt =
        receipt_with_git_for_test(workspace.path(), OsStr::new("definitely-not-a-git-binary"));
    assert_eq!(receipt.coverage, WorkspaceReceiptCoverage::Complete);
    let tracker = WorkspaceReceiptTracker::default();
    tracker.install_initial_receipt(receipt.clone());
    let lease = tracker.begin_foreground();

    tracker.invalidate();
    let mutation = lease
        .finish(receipt)
        .expect("interleaving forces an assumed mutation");

    assert!(mutation.contains("reason=concurrent_or_interleaved_mutation"));
}

#[test]
fn unchanged_complete_receipt_does_not_report_mutation() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::create_dir(workspace.path().join(".git")).expect("repository marker");
    std::fs::write(workspace.path().join("source.rs"), b"stable").expect("source");
    let receipt =
        receipt_with_git_for_test(workspace.path(), OsStr::new("definitely-not-a-git-binary"));
    assert_eq!(receipt.coverage, WorkspaceReceiptCoverage::Complete);
    let tracker = WorkspaceReceiptTracker::default();
    tracker.install_initial_receipt(receipt.clone());
    let lease = tracker.begin_foreground();
    assert_eq!(lease.finish(receipt), None);
}

#[tokio::test]
async fn unknown_before_receipt_skips_the_after_walk_and_assumes_mutation() {
    let workspace = tempfile::tempdir().expect("workspace");
    let tracker = WorkspaceReceiptTracker::default();
    tracker.install_initial_receipt(super::WorkspaceStateReceipt::unknown(
        WorkspaceReceiptStrategy::RepositoryWalk,
        WorkspaceReceiptUnknownReason::ContentLimit,
        12,
        super::WORKSPACE_RECEIPT_CONTENT_BUDGET_BYTES,
    ));
    let lease = tracker.begin_foreground();
    std::fs::remove_dir(workspace.path()).expect("remove receipt root");

    let mutation = lease
        .finish_for_root(workspace.path().to_path_buf())
        .await
        .expect("unknown coverage assumes mutation");

    assert!(mutation.contains("reason=content_limit"));
    assert!(mutation.contains("before_entries=12"));
    assert!(mutation.contains("after_entries=12"));
}

#[tokio::test]
async fn invalidated_complete_receipt_skips_the_after_walk_and_assumes_mutation() {
    let workspace = tempfile::tempdir().expect("workspace");
    let tracker = WorkspaceReceiptTracker::default();
    let receipt = super::WorkspaceStateReceipt {
        fingerprint: "blake3:known".into(),
        coverage: WorkspaceReceiptCoverage::Complete,
        strategy: WorkspaceReceiptStrategy::RepositoryWalk,
        counts_known: true,
        entries_visited: 1,
        content_bytes_read: 1,
    };
    tracker.install_initial_receipt(receipt);
    let lease = tracker.begin_foreground();
    tracker.invalidate();
    std::fs::remove_dir(workspace.path()).expect("remove receipt root");

    let mutation = lease
        .finish_for_root(workspace.path().to_path_buf())
        .await
        .expect("interleaving assumes mutation");

    assert!(mutation.contains("reason=concurrent_or_interleaved_mutation"));
}

/// Seam guard: the tracker's receipt budget governs the initial (before)
/// receipt. With a zero budget it cannot be complete; ignoring the seam would
/// compute it under the production budget and make it complete.
#[tokio::test]
async fn tracker_budget_governs_the_initial_receipt() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::create_dir(workspace.path().join(".git")).expect("repository marker");
    std::fs::write(workspace.path().join("source.rs"), b"stable").expect("source");
    let mut tracker = WorkspaceReceiptTracker::default();
    tracker.set_wall_time_for_tests(Duration::ZERO);

    let lease = tracker
        .begin_foreground_for_root(workspace.path().to_path_buf())
        .await;

    assert_eq!(
        lease.before.coverage,
        WorkspaceReceiptCoverage::Unknown(WorkspaceReceiptUnknownReason::WallTimeLimit)
    );
    let before = lease.before.clone();
    assert!(lease.finish(before).is_some());
}

/// Seam guard: the tracker's receipt budget also governs the after receipt.
/// The before receipt is complete, so the after receipt is really computed;
/// with a zero budget it is a wall-time `Unknown` and assumes a mutation.
#[tokio::test]
async fn tracker_budget_governs_the_after_receipt() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::create_dir(workspace.path().join(".git")).expect("repository marker");
    std::fs::write(workspace.path().join("source.rs"), b"stable").expect("source");
    let complete = super::compute_workspace_state_receipt_async_with_deadline(
        workspace.path().to_path_buf(),
        "definitely-not-a-git-binary".into(),
        test_deadline(),
    )
    .await;
    assert_eq!(complete.coverage, WorkspaceReceiptCoverage::Complete);
    let mut tracker = WorkspaceReceiptTracker::default();
    tracker.set_wall_time_for_tests(Duration::ZERO);
    tracker.install_initial_receipt(complete);
    let lease = tracker.begin_foreground();

    let mutation = lease
        .finish_for_root(workspace.path().to_path_buf())
        .await
        .expect("an exhausted after-receipt budget assumes mutation");

    assert!(mutation.contains("reason=wall_time_limit"), "{mutation}");
    assert!(mutation.contains("assumed_mutation=true"));
}
