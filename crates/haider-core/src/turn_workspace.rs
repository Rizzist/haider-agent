//! Complete tree receipts for a turn's workspace, independent of Git state.
//!
//! Each capture reads every included regular-file byte. File buffers are bounded,
//! but the retained path/digest map necessarily grows with the complete tree.
//! Capture runs outside the async executor; it neither adds a deadline nor treats
//! unreadable or concurrently changing entries as evidence of an untouched tree.

use std::collections::BTreeMap;
use std::fs::{self, Metadata};
use std::io::Read as _;
use std::path::{Path, PathBuf};

use haider_protocol::error::{ErrorCode, HaiderError};
use serde::{Deserialize, Serialize};

const RECEIPT_DOMAIN: &[u8] = b"haider.turn-workspace-tree.v1";
const CONTENT_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TreeReceipt {
    entries: BTreeMap<String, TreeEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TreeEntry {
    kind: EntryKind,
    digest: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum EntryKind {
    Directory,
    File,
    Symlink,
}

impl TreeReceipt {
    pub(crate) fn digest(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(RECEIPT_DOMAIN);
        for (path, entry) in &self.entries {
            hash_field(&mut hasher, path.as_bytes());
            hash_field(&mut hasher, entry.digest.as_bytes());
        }
        format!("blake3:{}", hasher.finalize().to_hex())
    }

    /// Created or changed files/symlinks, ordered by workspace-relative path.
    /// Directory-only changes affect the tree identity without inventing files.
    pub(crate) fn files_written(&self, post: &Self) -> Vec<String> {
        post.entries
            .iter()
            .filter(|(path, entry)| {
                entry.kind != EntryKind::Directory && self.entries.get(*path) != Some(*entry)
            })
            .map(|(path, _)| path.clone())
            .collect()
    }

    /// Removed files/symlinks, including a file replaced with a directory.
    pub(crate) fn files_deleted(&self, post: &Self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|(path, entry)| {
                entry.kind != EntryKind::Directory
                    && post
                        .entries
                        .get(*path)
                        .is_none_or(|after| after.kind == EntryKind::Directory)
            })
            .map(|(path, _)| path.clone())
            .collect()
    }

    pub(crate) fn is_same(&self, post: &Self) -> bool {
        self == post
    }
}

#[cfg(test)]
pub(crate) async fn capture(root: PathBuf) -> Result<TreeReceipt, HaiderError> {
    capture_cancellable(root, crate::CancelToken::new()).await
}

pub(crate) async fn capture_cancellable(
    root: PathBuf,
    cancel: crate::CancelToken,
) -> Result<TreeReceipt, HaiderError> {
    let worker_cancel = cancel.clone();
    let worker = tokio::task::spawn_blocking(move || capture_tree(&root, &worker_cancel));
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(receipt_error(Path::new("."), "capture tree", "cancelled")),
        result = worker => result.map_err(|error| receipt_error(Path::new("."), "receipt worker", error))?,
    }
}

fn check_cancel(cancel: &crate::CancelToken) -> Result<(), HaiderError> {
    if cancel.is_cancelled() {
        Err(receipt_error(Path::new("."), "capture tree", "cancelled"))
    } else {
        Ok(())
    }
}

fn capture_tree(root: &Path, cancel: &crate::CancelToken) -> Result<TreeReceipt, HaiderError> {
    capture_tree_observed(root, cancel, || {})
}

fn capture_tree_observed(
    root: &Path,
    cancel: &crate::CancelToken,
    mut chunk_read: impl FnMut(),
) -> Result<TreeReceipt, HaiderError> {
    check_cancel(cancel)?;
    let root_metadata = metadata(root)?;
    if !root_metadata.is_dir() || root_metadata.file_type().is_symlink() {
        return Err(receipt_error(
            root,
            "open workspace",
            "root is not a real directory",
        ));
    }
    let anchor = haider_platform::open_workspace_directory(root)
        .map_err(|error| receipt_error(root, "anchor workspace", error))?;
    let mut entries = BTreeMap::new();
    let mut directories = vec![PathBuf::new()];
    while let Some(relative) = directories.pop() {
        check_cancel(cancel)?;
        let path = root.join(&relative);
        let before = metadata(&path)?;
        if !before.is_dir() || before.file_type().is_symlink() {
            return Err(receipt_error(
                &path,
                "enumerate directory",
                "entry changed type",
            ));
        }
        entries.insert(
            path_key(&relative)?,
            entry_digest(EntryKind::Directory, &before, &[]),
        );
        let children = fs::read_dir(&path)
            .map_err(|error| receipt_error(&path, "enumerate directory", error))?;
        for child in children {
            check_cancel(cancel)?;
            let child =
                child.map_err(|error| receipt_error(&path, "read directory entry", error))?;
            let child_relative = relative.join(child.file_name());
            let child_path = root.join(&child_relative);
            let child_metadata = metadata(&child_path)?;
            // Git's administrative directory or linked-worktree file is not
            // part of the workspace tree. No other ignore/hidden rule applies.
            if child.file_name() == ".git"
                && (child_metadata.is_dir() || child_metadata.is_file())
                && !child_metadata.file_type().is_symlink()
            {
                continue;
            }
            if child_metadata.file_type().is_symlink() {
                let target = fs::read_link(&child_path)
                    .map_err(|error| receipt_error(&child_path, "read symlink target", error))?;
                ensure_stable(&child_path, &child_metadata, &metadata(&child_path)?)?;
                entries.insert(
                    path_key(&child_relative)?,
                    entry_digest(
                        EntryKind::Symlink,
                        &child_metadata,
                        target.as_os_str().as_encoded_bytes(),
                    ),
                );
            } else if child_metadata.is_dir() {
                directories.push(child_relative);
            } else if child_metadata.is_file() {
                let directory =
                    haider_platform::duplicate_workspace_directory(&anchor).map_err(|error| {
                        receipt_error(&child_path, "duplicate workspace anchor", error)
                    })?;
                let mut file = haider_platform::open_workspace_file(directory, &child_relative)
                    .map_err(|error| receipt_error(&child_path, "open regular file", error))?;
                let opened = file
                    .metadata()
                    .map_err(|error| receipt_error(&child_path, "inspect open file", error))?;
                ensure_stable(&child_path, &child_metadata, &opened)?;
                let mut content = blake3::Hasher::new();
                let mut buffer = [0_u8; CONTENT_BUFFER_BYTES];
                let mut remaining = opened.len();
                // Read exactly the observed length. A writer cannot make this
                // traversal chase an indefinitely growing file; growth fails
                // the final metadata check instead of yielding a partial hash.
                while remaining != 0 {
                    check_cancel(cancel)?;
                    let chunk = usize::try_from(remaining)
                        .unwrap_or(usize::MAX)
                        .min(buffer.len());
                    file.read_exact(&mut buffer[..chunk])
                        .map_err(|error| receipt_error(&child_path, "read complete file", error))?;
                    chunk_read();
                    content.update(&buffer[..chunk]);
                    remaining -= chunk as u64;
                }
                let after = file
                    .metadata()
                    .map_err(|error| receipt_error(&child_path, "inspect completed file", error))?;
                ensure_stable(&child_path, &opened, &after)?;
                ensure_stable(&child_path, &opened, &metadata(&child_path)?)?;
                entries.insert(
                    path_key(&child_relative)?,
                    entry_digest(EntryKind::File, &opened, content.finalize().as_bytes()),
                );
            } else {
                return Err(receipt_error(
                    &child_path,
                    "capture entry",
                    "unsupported special file",
                ));
            }
        }
        ensure_stable(&path, &before, &metadata(&path)?)?;
    }
    ensure_stable(root, &root_metadata, &metadata(root)?)?;
    Ok(TreeReceipt { entries })
}

fn path_key(relative: &Path) -> Result<String, HaiderError> {
    if relative.as_os_str().is_empty() {
        return Ok(".".into());
    }
    relative
        .to_str()
        .map(|path| {
            if cfg!(windows) {
                path.replace('\\', "/")
            } else {
                path.to_owned()
            }
        })
        .ok_or_else(|| receipt_error(relative, "encode relative path", "path is not valid UTF-8"))
}

fn metadata(path: &Path) -> Result<Metadata, HaiderError> {
    fs::symlink_metadata(path).map_err(|error| receipt_error(path, "inspect entry", error))
}

fn entry_digest(kind: EntryKind, metadata: &Metadata, content: &[u8]) -> TreeEntry {
    let mut hasher = blake3::Hasher::new();
    hasher.update(RECEIPT_DOMAIN);
    hash_field(
        &mut hasher,
        match kind {
            EntryKind::Directory => b"directory",
            EntryKind::File => b"file",
            EntryKind::Symlink => b"symlink",
        },
    );
    hash_permissions(&mut hasher, metadata);
    hash_field(&mut hasher, content);
    TreeEntry {
        kind,
        digest: format!("blake3:{}", hasher.finalize().to_hex()),
    }
}

fn hash_field(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

#[cfg(unix)]
fn hash_permissions(hasher: &mut blake3::Hasher, metadata: &Metadata) {
    use std::os::unix::fs::MetadataExt as _;
    hasher.update(&metadata.mode().to_be_bytes());
    hasher.update(&metadata.uid().to_be_bytes());
    hasher.update(&metadata.gid().to_be_bytes());
}

#[cfg(windows)]
fn hash_permissions(hasher: &mut blake3::Hasher, metadata: &Metadata) {
    hasher.update(&[u8::from(metadata.permissions().readonly())]);
}

fn ensure_stable(path: &Path, before: &Metadata, after: &Metadata) -> Result<(), HaiderError> {
    if same_observation(before, after) {
        Ok(())
    } else {
        Err(receipt_error(
            path,
            "capture entry",
            "entry changed during receipt",
        ))
    }
}

// Timestamps detect races within a capture, but are deliberately absent from
// the retained identity: touching/restoring a file is not a content mutation.
#[cfg(unix)]
fn same_observation(before: &Metadata, after: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    before.dev() == after.dev()
        && before.ino() == after.ino()
        && before.mode() == after.mode()
        && before.uid() == after.uid()
        && before.gid() == after.gid()
        && before.len() == after.len()
        && before.mtime() == after.mtime()
        && before.mtime_nsec() == after.mtime_nsec()
        && before.ctime() == after.ctime()
        && before.ctime_nsec() == after.ctime_nsec()
}

#[cfg(windows)]
fn same_observation(before: &Metadata, after: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;
    before.file_attributes() == after.file_attributes()
        && before.file_size() == after.file_size()
        && before.creation_time() == after.creation_time()
        && before.last_write_time() == after.last_write_time()
}

fn receipt_error(path: &Path, action: &str, error: impl std::fmt::Display) -> HaiderError {
    HaiderError::new(
        ErrorCode::Internal,
        format!(
            "workspace tree receipt failed to {action} at {}: {error}",
            path.display()
        ),
        false,
    )
}

#[cfg(test)]
#[path = "turn_workspace_tests.rs"]
mod tests;
