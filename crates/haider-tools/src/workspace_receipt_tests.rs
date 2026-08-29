#![allow(clippy::expect_used)]

use super::{
    WorkspaceReceiptCoverage, WorkspaceReceiptStrategy, WorkspaceReceiptTracker,
    WorkspaceReceiptUnknownReason, compute_workspace_state_receipt_with_git, porcelain_v1_paths,
    workspace_state_receipt,
};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

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

    let receipt = workspace_state_receipt(workspace.path());

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

    let receipt = compute_workspace_state_receipt_with_git(&workspace, fake_git.as_os_str());

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

    let before = compute_workspace_state_receipt_with_git(&workspace, fake_git.as_os_str());
    std::fs::write(workspace.join("source.rs"), "after").expect("mutate source");
    let after = compute_workspace_state_receipt_with_git(&workspace, fake_git.as_os_str());

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
    std::fs::write(workspace.path().join("source.rs"), b"before").expect("source");

    let before = compute_workspace_state_receipt_with_git(
        workspace.path(),
        OsStr::new("definitely-not-a-git-binary"),
    );
    std::fs::write(workspace.path().join("source.rs"), b"after").expect("mutate source");
    let after = compute_workspace_state_receipt_with_git(
        workspace.path(),
        OsStr::new("definitely-not-a-git-binary"),
    );

    assert_eq!(before.strategy, WorkspaceReceiptStrategy::RepositoryWalk);
    assert_eq!(before.coverage, WorkspaceReceiptCoverage::Complete);
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

    let receipt = workspace_state_receipt(workspace.path());

    assert_eq!(receipt.strategy, WorkspaceReceiptStrategy::GitStatus);
    assert_eq!(receipt.coverage, WorkspaceReceiptCoverage::Complete);
}

#[test]
fn observed_length_content_budget_is_explicitly_elided() {
    let workspace = tempfile::tempdir().expect("workspace");
    let oversized = workspace.path().join("oversized.bin");
    let file = std::fs::File::create(&oversized).expect("oversized file");
    file.set_len(super::WORKSPACE_RECEIPT_CONTENT_BUDGET_BYTES + 1)
        .expect("sparse size");
    drop(file);
    let reader = super::AnchoredWorkspaceReader::new(workspace.path()).expect("anchored reader");
    let mut builder = super::ReceiptBuilder::new(
        WorkspaceReceiptStrategy::RepositoryWalk,
        std::time::Instant::now() + std::time::Duration::from_secs(5),
    );

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
    assert!(receipt.mutation_digest().contains("reason=content_limit"));
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

    let receipt = compute_workspace_state_receipt_with_git(
        workspace.path(),
        OsStr::new("definitely-not-a-git-binary"),
    );

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
    let receipt = compute_workspace_state_receipt_with_git(
        workspace.path(),
        OsStr::new("definitely-not-a-git-binary"),
    );
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
