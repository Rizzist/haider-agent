//! Checkpoint capture and guarded workspace restoration.
//!
//! Capture consumes the exact in-memory pre-image collected by the mutation
//! worker. Restoration verifies every path before staging or publishing any
//! replacement, so a freshness conflict is all-or-nothing. Publication uses
//! same-directory temporary files plus directory sync; if a later I/O error
//! occurs, already-published paths are restored from the verified snapshots.

use crate::{CasSink, ToolError, ToolResult};
use haider_protocol::checkpoint::{
    CHECKPOINT_PREIMAGE_MAX_BYTES, CheckpointConflict, CheckpointKind, CheckpointOrigin,
    CheckpointPath, CheckpointRecorded, CheckpointRollbackConflict,
};
use haider_protocol::ids::{BranchId, CheckpointId, EffectId, RunId, SessionId};
use std::fs;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone)]
pub struct CheckpointCapturePath {
    pub path: String,
    /// `None` with no reason is an explicit absent marker.
    pub pre_bytes: Option<Vec<u8>>,
    pub pre_digest: Option<String>,
    pub post_digest: Option<String>,
    pub truncated_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CheckpointCapture {
    pub kind: CheckpointKind,
    pub paths: Vec<CheckpointCapturePath>,
    pub post_digest: String,
}

pub struct FreezeCheckpointInput {
    pub session_id: SessionId,
    pub branch_id: Option<BranchId>,
    pub run_id: RunId,
    pub effect_id: EffectId,
    pub call_id: String,
    pub origin: CheckpointOrigin,
    pub source_checkpoint_id: Option<CheckpointId>,
}

/// Freezes every bounded pre-image before constructing the journal fact.
/// Content addressing naturally deduplicates identical bytes.
pub async fn freeze_checkpoint<C: CasSink + ?Sized>(
    cas: &mut C,
    input: FreezeCheckpointInput,
    capture: CheckpointCapture,
) -> ToolResult<CheckpointRecorded> {
    let mut paths = Vec::with_capacity(capture.paths.len());
    for captured in capture.paths {
        let (pre_artifact, truncated_reason) = match captured.pre_bytes {
            Some(bytes)
                if u64::try_from(bytes.len()).unwrap_or(u64::MAX)
                    <= CHECKPOINT_PREIMAGE_MAX_BYTES =>
            {
                (Some(cas.put(&bytes).await?), captured.truncated_reason)
            }
            Some(bytes) => (
                None,
                Some(format!(
                    "pre-image is {} bytes; checkpoint limit is {} bytes",
                    bytes.len(),
                    CHECKPOINT_PREIMAGE_MAX_BYTES
                )),
            ),
            None => (None, captured.truncated_reason),
        };
        paths.push(CheckpointPath {
            path: captured.path,
            pre_artifact,
            pre_digest: captured.pre_digest,
            post_digest: captured.post_digest,
            truncated_reason,
        });
    }
    let checkpoint_id = checkpoint_id(&input, &capture.post_digest);
    Ok(CheckpointRecorded {
        checkpoint_id,
        session_id: input.session_id,
        branch_id: input.branch_id,
        run_id: input.run_id,
        effect_id: input.effect_id,
        call_id: input.call_id,
        seq: 0,
        workspace_revision: None,
        kind: capture.kind,
        origin: input.origin,
        source_checkpoint_id: input.source_checkpoint_id,
        paths,
        post_digest: capture.post_digest,
        recorded_at_ms: 0,
    })
}

pub(crate) fn checkpoint_without_cas(
    input: FreezeCheckpointInput,
    capture: CheckpointCapture,
    reason: &str,
) -> CheckpointRecorded {
    let paths = capture
        .paths
        .into_iter()
        .map(|captured| CheckpointPath {
            path: captured.path,
            pre_artifact: None,
            pre_digest: captured.pre_digest,
            post_digest: captured.post_digest,
            truncated_reason: captured
                .pre_bytes
                .map(|bytes| {
                    format!(
                        "pre-image is {} bytes but no checkpoint artifact was stored: {reason}",
                        bytes.len(),
                    )
                })
                .or(captured.truncated_reason),
        })
        .collect();
    let checkpoint_id = checkpoint_id(&input, &capture.post_digest);
    CheckpointRecorded {
        checkpoint_id,
        session_id: input.session_id,
        branch_id: input.branch_id,
        run_id: input.run_id,
        effect_id: input.effect_id,
        call_id: input.call_id,
        seq: 0,
        workspace_revision: None,
        kind: capture.kind,
        origin: input.origin,
        source_checkpoint_id: input.source_checkpoint_id,
        paths,
        post_digest: capture.post_digest,
        recorded_at_ms: 0,
    }
}

fn checkpoint_id(input: &FreezeCheckpointInput, post_digest: &str) -> CheckpointId {
    let mut hasher = blake3::Hasher::new();
    for part in [
        input.session_id.as_str(),
        input.branch_id.as_ref().map_or("", BranchId::as_str),
        input.run_id.as_str(),
        input.effect_id.as_str(),
        &input.call_id,
        post_digest,
    ] {
        let part_len = u64::try_from(part.len()).unwrap_or(u64::MAX);
        hasher.update(&part_len.to_be_bytes());
        hasher.update(part.as_bytes());
    }
    CheckpointId::new(format!("checkpoint:{}", hasher.finalize().to_hex()))
}

#[derive(Debug, Clone)]
pub struct CheckpointRestoreTarget {
    pub path: String,
    /// Digest required before applying this direction. `None` means absent.
    pub expected_digest: Option<String>,
    /// Exact state to install. `None` means remove the path.
    pub restore_bytes: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct CheckpointRestorePlan {
    pub workspace_root: PathBuf,
    pub targets: Vec<CheckpointRestoreTarget>,
}

#[derive(Debug)]
pub enum CheckpointRestoreError {
    Conflict(CheckpointRollbackConflict),
    Tool(ToolError),
}

impl std::fmt::Display for CheckpointRestoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict(conflict) => write!(
                formatter,
                "checkpoint freshness conflict on {} path(s)",
                conflict.conflicts.len()
            ),
            Self::Tool(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for CheckpointRestoreError {}

impl From<ToolError> for CheckpointRestoreError {
    fn from(error: ToolError) -> Self {
        Self::Tool(error)
    }
}

/// Verifies every target and returns all conflicts without changing disk.
pub fn verify_checkpoint_restore_plan(
    plan: &CheckpointRestorePlan,
) -> Result<(), CheckpointRestoreError> {
    let root = fs::canonicalize(&plan.workspace_root).map_err(|error| {
        ToolError::io(
            "canonicalize checkpoint workspace",
            &plan.workspace_root,
            error,
        )
    })?;
    let mut verified = Vec::new();
    let mut conflicts = Vec::new();
    for target in &plan.targets {
        let path = checked_target(&root, &target.path)?;
        match digest_if_file(&path) {
            Ok(current_digest) if current_digest == target.expected_digest => {
                verified.push(target.path.clone());
            }
            Ok(current_digest) => conflicts.push(CheckpointConflict {
                path: target.path.clone(),
                expected_digest: target.expected_digest.clone(),
                current_digest,
            }),
            Err(ToolError::PathChanged { .. }) => conflicts.push(CheckpointConflict {
                path: target.path.clone(),
                expected_digest: target.expected_digest.clone(),
                current_digest: None,
            }),
            Err(error) => return Err(error.into()),
        }
    }
    if conflicts.is_empty() {
        Ok(())
    } else {
        Err(CheckpointRestoreError::Conflict(
            CheckpointRollbackConflict {
                verified,
                conflicts,
            },
        ))
    }
}

/// Applies a previously constructed plan. Freshness is rechecked immediately
/// before staging, and every current state is retained for error rollback.
pub fn restore_checkpoint_plan(
    plan: &CheckpointRestorePlan,
) -> Result<Vec<CheckpointCapturePath>, CheckpointRestoreError> {
    verify_checkpoint_restore_plan(plan)?;
    let root = fs::canonicalize(&plan.workspace_root).map_err(|error| {
        ToolError::io(
            "canonicalize checkpoint workspace",
            &plan.workspace_root,
            error,
        )
    })?;
    let mut captures = Vec::with_capacity(plan.targets.len());
    for target in &plan.targets {
        let path = checked_target(&root, &target.path)?;
        let current = match locked_current(&path) {
            Ok(current) => current.map(|current| current.bytes),
            Err(ToolError::PathChanged { .. }) => {
                return Err(CheckpointRestoreError::Conflict(
                    CheckpointRollbackConflict {
                        verified: captures
                            .iter()
                            .map(|capture: &CheckpointCapturePath| capture.path.clone())
                            .collect(),
                        conflicts: vec![CheckpointConflict {
                            path: target.path.clone(),
                            expected_digest: target.expected_digest.clone(),
                            current_digest: None,
                        }],
                    },
                ));
            }
            Err(error) => return Err(error.into()),
        };
        let current_digest = current.as_deref().map(digest);
        if current_digest != target.expected_digest {
            return Err(CheckpointRestoreError::Conflict(
                CheckpointRollbackConflict {
                    verified: captures
                        .iter()
                        .map(|capture: &CheckpointCapturePath| capture.path.clone())
                        .collect(),
                    conflicts: vec![CheckpointConflict {
                        path: target.path.clone(),
                        expected_digest: target.expected_digest.clone(),
                        current_digest,
                    }],
                },
            ));
        }
        captures.push(CheckpointCapturePath {
            path: target.path.clone(),
            pre_digest: target.expected_digest.clone(),
            pre_bytes: current,
            post_digest: target.restore_bytes.as_deref().map(digest),
            truncated_reason: None,
        });
    }

    let mut applied = 0usize;
    for target in &plan.targets {
        let path = checked_target(&root, &target.path)?;
        // Count the current target before publication: `install_state` can
        // fail during the post-replace directory sync, after bytes changed.
        // Recovery must therefore include it even on an error return.
        applied += 1;
        match install_state(
            &root,
            &target.path,
            target.expected_digest.as_deref(),
            target.restore_bytes.as_deref(),
        ) {
            Ok(()) => {}
            Err(InstallStateError::Conflict { current_digest }) => {
                applied -= 1;
                rollback_applied(&root, &plan.targets, &captures, applied)?;
                return Err(CheckpointRestoreError::Conflict(
                    CheckpointRollbackConflict {
                        verified: plan.targets[..applied]
                            .iter()
                            .map(|target| target.path.clone())
                            .collect(),
                        conflicts: vec![CheckpointConflict {
                            path: target.path.clone(),
                            expected_digest: target.expected_digest.clone(),
                            current_digest,
                        }],
                    },
                ));
            }
            Err(InstallStateError::Tool(error)) => {
                let installed_digest = target.restore_bytes.as_deref().map(digest);
                match digest_if_file(&path) {
                    Ok(current_digest) if current_digest == installed_digest => {}
                    Ok(_) => applied -= 1,
                    Err(recheck_error) => {
                        applied -= 1;
                        rollback_applied(&root, &plan.targets, &captures, applied)?;
                        return Err(recheck_error.into());
                    }
                }
                rollback_applied(&root, &plan.targets, &captures, applied)?;
                return Err(error.into());
            }
        }
    }
    Ok(captures)
}

fn rollback_applied(
    root: &Path,
    targets: &[CheckpointRestoreTarget],
    captures: &[CheckpointCapturePath],
    applied: usize,
) -> Result<(), CheckpointRestoreError> {
    let mut failures = Vec::new();
    for (target, capture) in targets[..applied].iter().zip(&captures[..applied]).rev() {
        let attempt = (|| -> Result<(), CheckpointRestoreError> {
            let path = checked_target(root, &target.path)?;
            let applied_digest = target.restore_bytes.as_deref().map(digest);
            if digest_if_file(&path)? != applied_digest {
                return Err(ToolError::PathChanged {
                    path,
                    message: "checkpoint rollback refused a concurrent foreign edit".into(),
                }
                .into());
            }
            match install_state(
                root,
                &target.path,
                applied_digest.as_deref(),
                capture.pre_bytes.as_deref(),
            ) {
                Ok(()) => Ok(()),
                Err(InstallStateError::Conflict { .. }) => Err(ToolError::PathChanged {
                    path,
                    message: "checkpoint rollback refused a concurrent foreign edit".into(),
                }
                .into()),
                Err(InstallStateError::Tool(error)) => Err(error.into()),
            }
        })();
        if let Err(error) = attempt {
            failures.push(format!("{}: {error}", target.path));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(ToolError::Runtime {
            message: format!(
                "checkpoint recovery attempted every applied path; {} failure(s): {}",
                failures.len(),
                failures.join("; ")
            ),
        }
        .into())
    }
}

fn checked_target(root: &Path, relative: &str) -> ToolResult<PathBuf> {
    let relative = Path::new(relative);
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(ToolError::WorkspaceBoundary {
            workspace_root: root.to_path_buf(),
            requested_path: relative.to_path_buf(),
            resolved_path: None,
        });
    }
    let path = root.join(relative);
    let parent = path.parent().ok_or_else(|| {
        ToolError::invalid_argument("checkpoint restore path has no parent directory")
    })?;
    let canonical_parent = fs::canonicalize(parent)
        .map_err(|error| ToolError::io("canonicalize checkpoint parent", parent, error))?;
    if !canonical_parent.starts_with(root) {
        return Err(ToolError::WorkspaceBoundary {
            workspace_root: root.to_path_buf(),
            requested_path: relative.to_path_buf(),
            resolved_path: Some(canonical_parent),
        });
    }
    Ok(canonical_parent.join(
        path.file_name().ok_or_else(|| {
            ToolError::invalid_argument("checkpoint restore path has no file name")
        })?,
    ))
}

fn digest_if_file(path: &Path) -> ToolResult<Option<String>> {
    locked_current(path).map(|current| current.map(|current| digest(&current.bytes)))
}

fn digest(bytes: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(bytes).to_hex())
}

pub(crate) enum InstallStateError {
    Conflict { current_digest: Option<String> },
    Tool(ToolError),
}

impl From<ToolError> for InstallStateError {
    fn from(error: ToolError) -> Self {
        Self::Tool(error)
    }
}

struct LockedCurrent {
    _file: fs::File,
    bytes: Vec<u8>,
}

fn locked_current(path: &Path) -> ToolResult<Option<LockedCurrent>> {
    let path_metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(ToolError::io("inspect checkpoint freshness", path, error));
        }
    };
    if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
        return Err(ToolError::PathChanged {
            path: path.to_path_buf(),
            message: "checkpoint restore target is not a regular file".into(),
        });
    }
    let mut file = fs::OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ToolError::PathChanged {
                    path: path.to_path_buf(),
                    message: "checkpoint target disappeared while acquiring its lock".into(),
                }
            } else {
                ToolError::io("open checkpoint freshness", path, error)
            }
        })?;
    file.lock()
        .map_err(|error| ToolError::io("lock checkpoint freshness", path, error))?;
    require_locked_path_identity(path, &file)?;
    file.seek(SeekFrom::Start(0))
        .map_err(|error| ToolError::io("seek checkpoint freshness", path, error))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| ToolError::io("read checkpoint freshness", path, error))?;
    require_locked_path_identity(path, &file)?;
    Ok(Some(LockedCurrent { _file: file, bytes }))
}

#[cfg(unix)]
fn require_locked_path_identity(path: &Path, file: &fs::File) -> ToolResult<()> {
    use std::os::unix::fs::MetadataExt as _;

    let locked = file
        .metadata()
        .map_err(|error| ToolError::io("identify locked checkpoint target", path, error))?;
    let current = fs::symlink_metadata(path).map_err(|error| ToolError::PathChanged {
        path: path.to_path_buf(),
        message: format!("checkpoint target changed while locked: {error}"),
    })?;
    if !current.file_type().is_symlink()
        && current.is_file()
        && locked.dev() == current.dev()
        && locked.ino() == current.ino()
    {
        return Ok(());
    }
    Err(ToolError::PathChanged {
        path: path.to_path_buf(),
        message: "checkpoint target identity changed while acquiring its lock".into(),
    })
}

#[cfg(windows)]
fn require_locked_path_identity(path: &Path, file: &fs::File) -> ToolResult<()> {
    let locked = haider_platform::windows_file_identity(file)
        .map_err(|error| ToolError::io("identify locked checkpoint target", path, error))?;
    let current = fs::OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(|error| ToolError::PathChanged {
            path: path.to_path_buf(),
            message: format!("checkpoint target changed while locked: {error}"),
        })?;
    let current = haider_platform::windows_file_identity(&current)
        .map_err(|error| ToolError::io("reidentify checkpoint target", path, error))?;
    if locked == current {
        return Ok(());
    }
    Err(ToolError::PathChanged {
        path: path.to_path_buf(),
        message: "checkpoint target identity changed while acquiring its lock".into(),
    })
}

fn install_state(
    workspace_root: &Path,
    relative: &str,
    expected_digest: Option<&str>,
    bytes: Option<&[u8]>,
) -> Result<(), InstallStateError> {
    crate::filesystem::install_checkpoint_state(
        workspace_root,
        Path::new(relative),
        expected_digest,
        bytes,
    )
}
