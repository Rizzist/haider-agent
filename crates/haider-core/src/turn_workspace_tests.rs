#![allow(clippy::expect_used)]

use super::*;

/// Cancelling after one chunk must stop the blocking worker before the rest
/// of a large file is read; this does not rely on a timing threshold.
#[test]
fn receipt_worker_stops_between_file_chunks_on_cancellation() {
    let workspace = tempfile::tempdir().expect("workspace");
    let file = fs::File::create(workspace.path().join("large-file")).expect("file");
    file.set_len((CONTENT_BUFFER_BYTES * 8) as u64)
        .expect("sparse length");
    let cancel = crate::CancelToken::new();
    let mut chunks = 0;
    let result = capture_tree_observed(workspace.path(), &cancel, || {
        chunks += 1;
        cancel.cancel();
    });
    assert!(
        result
            .expect_err("cancelled receipt")
            .message
            .contains("cancelled")
    );
    assert_eq!(chunks, 1);
}

#[tokio::test]
async fn cancelled_receipt_await_returns_without_observing_the_tree() {
    let workspace = tempfile::tempdir().expect("workspace");
    let cancel = crate::CancelToken::new();
    cancel.cancel();
    let error = capture_cancellable(workspace.path().into(), cancel)
        .await
        .expect_err("cancelled");
    assert!(error.message.contains("cancelled"));
}

#[tokio::test]
async fn complete_nonrepository_receipt_preserves_unchanged_preexisting_dirt() {
    let workspace = tempfile::tempdir().expect("workspace");
    fs::write(workspace.path().join("preexisting.txt"), "uncommitted text").expect("dirty file");
    let before = capture(workspace.path().into()).await.expect("before");
    let post = capture(workspace.path().into()).await.expect("after");
    assert!(before.is_same(&post));
    assert_eq!(before.digest(), post.digest());
    assert!(before.files_written(&post).is_empty());
    assert!(before.files_deleted(&post).is_empty());
    let encoded = serde_json::to_string(&before).expect("serialize receipt");
    let retained: TreeReceipt = serde_json::from_str(&encoded).expect("deserialize receipt");
    assert_eq!(before, retained);
}

#[tokio::test]
async fn complete_receipt_detects_same_size_edit_and_orders_created_and_deleted_files() {
    let workspace = tempfile::tempdir().expect("workspace");
    fs::write(workspace.path().join("z-edit"), "old").expect("original");
    fs::write(workspace.path().join("deleted"), "gone").expect("deleted original");
    let before = capture(workspace.path().into()).await.expect("before");
    fs::write(workspace.path().join("z-edit"), "new").expect("same length edit");
    fs::write(workspace.path().join("a-created"), "created").expect("created file");
    fs::remove_file(workspace.path().join("deleted")).expect("delete file");
    let post = capture(workspace.path().into()).await.expect("after");
    assert!(!before.is_same(&post));
    assert_ne!(before.digest(), post.digest());
    assert_eq!(before.files_written(&post), ["a-created", "z-edit"]);
    assert_eq!(before.files_deleted(&post), ["deleted"]);
}

#[tokio::test]
async fn complete_receipt_includes_ignored_and_hidden_entries_and_excludes_git_metadata() {
    let workspace = tempfile::tempdir().expect("workspace");
    let root = workspace.path().join(".hidden-root");
    fs::create_dir(&root).expect("hidden root");
    fs::write(root.join(".gitignore"), "ignored/\n").expect("ignore file");
    fs::create_dir(root.join(".git")).expect("git metadata");
    fs::write(root.join(".git/config"), "old").expect("git config");
    fs::create_dir(root.join("ignored")).expect("ignored directory");
    fs::write(root.join("ignored/ignored.txt"), "old").expect("ignored file");
    let before = capture(root.clone()).await.expect("before");
    fs::write(root.join(".git/config"), "new").expect("git metadata edit");
    let git_only = capture(root.clone()).await.expect("git-only change");
    assert!(before.is_same(&git_only));
    fs::write(root.join("ignored/ignored.txt"), "new").expect("ignored edit");
    fs::write(root.join(".hidden-file"), "hidden").expect("hidden file");
    let post = capture(root).await.expect("after");
    assert_eq!(
        before.files_written(&post),
        [".hidden-file", "ignored/ignored.txt"]
    );
}

#[tokio::test]
async fn complete_receipt_tracks_empty_directories_without_inventing_written_files() {
    let workspace = tempfile::tempdir().expect("workspace");
    let before = capture(workspace.path().into()).await.expect("before");
    fs::create_dir(workspace.path().join("empty")).expect("empty directory");
    let post = capture(workspace.path().into()).await.expect("after");
    assert!(!before.is_same(&post));
    assert!(before.files_written(&post).is_empty());
    assert!(before.files_deleted(&post).is_empty());
}

#[tokio::test]
async fn complete_receipt_excludes_linked_worktree_git_file_and_detects_type_changes() {
    let workspace = tempfile::tempdir().expect("workspace");
    fs::write(workspace.path().join(".git"), "gitdir: outside").expect("git file");
    fs::write(workspace.path().join("replacement"), "file").expect("file");
    let before = capture(workspace.path().into()).await.expect("before");
    fs::write(workspace.path().join(".git"), "gitdir: different").expect("changed git file");
    let git_only = capture(workspace.path().into())
        .await
        .expect("git-only change");
    assert!(before.is_same(&git_only));
    fs::remove_file(workspace.path().join("replacement")).expect("remove file");
    fs::create_dir(workspace.path().join("replacement")).expect("replace with directory");
    let post = capture(workspace.path().into()).await.expect("after");
    assert!(!before.is_same(&post));
    assert_eq!(before.files_deleted(&post), ["replacement"]);
    assert!(before.files_written(&post).is_empty());
}

#[tokio::test]
async fn complete_receipt_ignores_content_restoration_timestamps() {
    let workspace = tempfile::tempdir().expect("workspace");
    let file = workspace.path().join("restored");
    fs::write(&file, "original").expect("original");
    let before = capture(workspace.path().into()).await.expect("before");
    fs::write(&file, "changed contents").expect("temporary change");
    fs::write(&file, "original").expect("restore");
    let post = capture(workspace.path().into()).await.expect("after");
    assert!(before.is_same(&post));
}

#[tokio::test]
async fn complete_receipt_missing_workspace_is_typed_error() {
    let workspace = tempfile::tempdir().expect("workspace");
    let error = capture(workspace.path().join("missing"))
        .await
        .expect_err("missing root");
    assert_eq!(error.code, ErrorCode::Internal);
    assert!(error.message.contains("workspace tree receipt failed"));
}

#[cfg(unix)]
#[tokio::test]
async fn complete_receipt_hashes_symlink_targets_without_following_them() {
    use std::os::unix::fs::symlink;
    let workspace = tempfile::tempdir().expect("workspace");
    let external = tempfile::tempdir().expect("external");
    fs::write(external.path().join("outside"), "old").expect("external file");
    symlink(external.path(), workspace.path().join("link")).expect("directory link");
    let before = capture(workspace.path().into()).await.expect("before");
    fs::write(external.path().join("outside"), "new").expect("external edit");
    let external_changed = capture(workspace.path().into())
        .await
        .expect("after external edit");
    assert!(before.is_same(&external_changed));
    fs::remove_file(workspace.path().join("link")).expect("remove link");
    symlink("missing-target", workspace.path().join("link")).expect("dangling link");
    let post = capture(workspace.path().into())
        .await
        .expect("after target change");
    assert_eq!(before.files_written(&post), ["link"]);
}

#[cfg(unix)]
#[tokio::test]
async fn complete_receipt_detects_permission_changes() {
    use std::os::unix::fs::PermissionsExt as _;
    let workspace = tempfile::tempdir().expect("workspace");
    let file = workspace.path().join("script");
    fs::write(&file, "script").expect("script");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).expect("initial mode");
    let before = capture(workspace.path().into()).await.expect("before");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o700)).expect("executable mode");
    let post = capture(workspace.path().into()).await.expect("after");
    assert_eq!(before.files_written(&post), ["script"]);
}
