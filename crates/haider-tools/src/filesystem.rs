//! Bounded filesystem tools executed through [`EffectBroker`].
//!
//! Owned invariants:
//! - Every tool runs inside the broker's begin/finish envelope, so the
//!   four-phase effect law covers reads and writes alike and nothing here
//!   touches the filesystem before a journaled `Allow` + `Dispatched`.
//! - Read-class results are bounded: an oversized result keeps a UTF-8-safe
//!   preview and freezes the complete payload in the CAS via [`CasSink`].
//! - `fs_patch` proves its pre-image before writing (mismatch is a typed
//!   [`FsPatchConflict`]) and records applied writes in the [`ChangeLedger`],
//!   attributed to the caller's `(session, turn)`, for the verify gate. The
//!   read/verify/replace sequence holds exclusive file locks, writes a
//!   same-directory temporary file, and atomically renames it over the target.
//! - Every caller-supplied path is resolved to a canonical path under the
//!   broker's canonical workspace root before it is digested or dispatched.

use crate::broker::{EffectBroker, EffectOperation, JournalSink, PermissionPolicy};
use crate::ledger::{ChangeLedger, FsWriteRecord};
use crate::{FsPatchConflict, ToolError, ToolResult};
use async_trait::async_trait;
use haider_protocol::effect::EffectClass;
use haider_protocol::ids::{ArtifactRef, RunId, SessionId};
use haider_protocol::tool::BoundedResult;
use same_file::Handle;
use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

/// Port for storing the complete result when its prompt preview is truncated.
#[async_trait]
pub trait CasSink: Send {
    async fn put(&mut self, bytes: &[u8]) -> ToolResult<ArtifactRef>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResultBounds {
    pub max_preview_bytes: usize,
}

impl Default for ResultBounds {
    fn default() -> Self {
        Self {
            max_preview_bytes: 8 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnAttribution {
    pub session: SessionId,
    pub turn: RunId,
}

impl TurnAttribution {
    pub fn new(session: SessionId, turn: RunId) -> Self {
        Self { session, turn }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsRead {
    pub path: PathBuf,
}

impl FsRead {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl EffectOperation for FsRead {
    fn effect_class(&self) -> EffectClass {
        EffectClass::FsRead
    }

    fn summary(&self) -> String {
        format!("read {}", self.path.display())
    }

    fn arguments(&self) -> ToolResult<Value> {
        Ok(json!({ "path": path_argument(&self.path)? }))
    }

    fn canonical_arguments(&self, workspace_root: &Path) -> ToolResult<Value> {
        let path = resolve_workspace_path(workspace_root, &self.path, PathResolution::Existing)?;
        Ok(json!({ "path": path_argument(&path)? }))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsList {
    pub path: PathBuf,
}

impl FsList {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl EffectOperation for FsList {
    fn effect_class(&self) -> EffectClass {
        EffectClass::FsRead
    }

    fn summary(&self) -> String {
        format!("list {}", self.path.display())
    }

    fn arguments(&self) -> ToolResult<Value> {
        Ok(json!({ "path": path_argument(&self.path)? }))
    }

    fn canonical_arguments(&self, workspace_root: &Path) -> ToolResult<Value> {
        let path = resolve_workspace_path(workspace_root, &self.path, PathResolution::Existing)?;
        Ok(json!({ "path": path_argument(&path)? }))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsSearch {
    pub root: PathBuf,
    pub query: String,
}

impl FsSearch {
    pub fn new(root: impl Into<PathBuf>, query: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            query: query.into(),
        }
    }
}

impl EffectOperation for FsSearch {
    fn effect_class(&self) -> EffectClass {
        EffectClass::FsRead
    }

    fn summary(&self) -> String {
        format!("search {} for {:?}", self.root.display(), self.query)
    }

    fn arguments(&self) -> ToolResult<Value> {
        Ok(json!({
            "query": self.query,
            "root": path_argument(&self.root)?,
        }))
    }

    fn canonical_arguments(&self, workspace_root: &Path) -> ToolResult<Value> {
        let root = resolve_workspace_path(workspace_root, &self.root, PathResolution::Existing)?;
        Ok(json!({
            "query": self.query,
            "root": path_argument(&root)?,
        }))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsPatch {
    pub path: PathBuf,
    pub preimage: String,
    pub replacement: String,
}

impl FsPatch {
    pub fn new(
        path: impl Into<PathBuf>,
        preimage: impl Into<String>,
        replacement: impl Into<String>,
    ) -> Self {
        Self {
            path: path.into(),
            preimage: preimage.into(),
            replacement: replacement.into(),
        }
    }
}

impl EffectOperation for FsPatch {
    fn effect_class(&self) -> EffectClass {
        EffectClass::FsWrite
    }

    fn summary(&self) -> String {
        format!("patch {}", self.path.display())
    }

    fn arguments(&self) -> ToolResult<Value> {
        Ok(json!({
            "path": path_argument(&self.path)?,
            "preimage": self.preimage,
            "replacement": self.replacement,
        }))
    }

    fn canonical_arguments(&self, workspace_root: &Path) -> ToolResult<Value> {
        let path =
            resolve_workspace_path(workspace_root, &self.path, PathResolution::MissingLeafOk)?;
        Ok(json!({
            "path": path_argument(&path)?,
            "preimage": self.preimage,
            "replacement": self.replacement,
        }))
    }
}

impl<J> EffectBroker<J>
where
    J: JournalSink,
{
    pub async fn fs_read<C>(
        &mut self,
        operation: &FsRead,
        policy: &PermissionPolicy,
        cas: &mut C,
        bounds: ResultBounds,
    ) -> ToolResult<BoundedResult>
    where
        C: CasSink,
    {
        let operation = FsRead::new(resolve_workspace_path(
            self.workspace_root(),
            &operation.path,
            PathResolution::Existing,
        )?);
        let path = operation.path.clone();
        self.bounded_read(&operation, policy, cas, bounds, move || read_utf8(&path))
            .await
    }

    pub async fn fs_list<C>(
        &mut self,
        operation: &FsList,
        policy: &PermissionPolicy,
        cas: &mut C,
        bounds: ResultBounds,
    ) -> ToolResult<BoundedResult>
    where
        C: CasSink,
    {
        let operation = FsList::new(resolve_workspace_path(
            self.workspace_root(),
            &operation.path,
            PathResolution::Existing,
        )?);
        let path = operation.path.clone();
        self.bounded_read(&operation, policy, cas, bounds, move || {
            list_directory(&path)
        })
        .await
    }

    pub async fn fs_search<C>(
        &mut self,
        operation: &FsSearch,
        policy: &PermissionPolicy,
        cas: &mut C,
        bounds: ResultBounds,
    ) -> ToolResult<BoundedResult>
    where
        C: CasSink,
    {
        let operation = FsSearch::new(
            resolve_workspace_path(
                self.workspace_root(),
                &operation.root,
                PathResolution::Existing,
            )?,
            operation.query.clone(),
        );
        let owned = operation.clone();
        let workspace_root = self.workspace_root().to_path_buf();
        self.bounded_read(&operation, policy, cas, bounds, move || {
            search_files(&owned, &workspace_root)
        })
        .await
    }

    /// Shared read-class envelope: begin (intent → authorize → dispatched),
    /// produce the raw text off the async runtime, apply the result bound,
    /// journal the outcome.
    async fn bounded_read<O, C, F>(
        &mut self,
        operation: &O,
        policy: &PermissionPolicy,
        cas: &mut C,
        bounds: ResultBounds,
        produce: F,
    ) -> ToolResult<BoundedResult>
    where
        O: EffectOperation,
        C: CasSink,
        F: FnOnce() -> ToolResult<String> + Send + 'static,
    {
        let intent = self.begin(operation, policy).await?;
        let result = match run_blocking(produce).await {
            Ok(contents) => bounded(contents, bounds, cas).await,
            Err(error) => Err(error),
        };
        self.finish(&intent, result).await
    }

    pub async fn fs_patch(
        &mut self,
        operation: &FsPatch,
        policy: &PermissionPolicy,
        attribution: &TurnAttribution,
        ledger: &mut ChangeLedger,
    ) -> ToolResult<BoundedResult> {
        let operation = FsPatch::new(
            resolve_workspace_path(
                self.workspace_root(),
                &operation.path,
                PathResolution::MissingLeafOk,
            )?,
            operation.preimage.clone(),
            operation.replacement.clone(),
        );
        let intent = self.begin(&operation, policy).await?;
        let owned_operation = operation.clone();
        let workspace_root = self.workspace_root().to_path_buf();
        let result = run_blocking(move || apply_patch(&owned_operation, &workspace_root)).await;
        let result = result.map(|applied| {
            // The atomic rename is real on disk at this point. Record evidence
            // made from the exact byte buffer written before attempting the
            // outcome append, which may still fail independently.
            ledger.record_fs_write(
                attribution.session.clone(),
                attribution.turn.clone(),
                FsWriteRecord {
                    effect: intent.effect.clone(),
                    paths: vec![applied.path],
                    summary: intent.summary.clone(),
                    bytes_hash: applied.bytes_hash,
                },
            );
            applied.result
        });
        self.finish(&intent, result).await
    }
}

fn read_utf8(path: &Path) -> ToolResult<String> {
    let bytes = fs::read(path).map_err(|error| ToolError::io("read", path, error))?;
    String::from_utf8(bytes).map_err(|error| ToolError::InvalidArgument {
        message: format!("{} is not UTF-8 text: {error}", path.display()),
    })
}

fn list_directory(path: &Path) -> ToolResult<String> {
    let entries = fs::read_dir(path).map_err(|error| ToolError::io("list", path, error))?;
    let mut listed = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| ToolError::io("list", path, error))?;
        let file_type = entry
            .file_type()
            .map_err(|error| ToolError::io("inspect", entry.path(), error))?;
        let mut name = entry.file_name().to_string_lossy().into_owned();
        if file_type.is_dir() {
            name.push('/');
        }
        listed.push(name);
    }
    listed.sort();
    Ok(join_lines(listed))
}

fn search_files(operation: &FsSearch, workspace_root: &Path) -> ToolResult<String> {
    if operation.query.is_empty() {
        return Err(ToolError::invalid_argument(
            "fs_search query cannot be empty",
        ));
    }
    let mut files = Vec::new();
    collect_files(&operation.root, workspace_root, &operation.root, &mut files)?;
    files.sort();

    let mut matches = Vec::new();
    for path in files {
        let bytes = fs::read(&path).map_err(|error| ToolError::io("read", &path, error))?;
        let Ok(contents) = std::str::from_utf8(&bytes) else {
            continue;
        };
        let display_path = path
            .strip_prefix(&operation.root)
            .ok()
            .filter(|relative| !relative.as_os_str().is_empty())
            .unwrap_or(&path);
        for (index, line) in contents.lines().enumerate() {
            if line.contains(&operation.query) {
                matches.push(format!("{}:{}:{}", display_path.display(), index + 1, line));
            }
        }
    }
    Ok(join_lines(matches))
}

fn collect_files(
    path: &Path,
    workspace_root: &Path,
    search_root: &Path,
    files: &mut Vec<PathBuf>,
) -> ToolResult<()> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| ToolError::io("inspect", path, error))?;
    // Symlinks are skipped entirely: following them risks cycles and reads
    // outside the search root.
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    let canonical =
        fs::canonicalize(path).map_err(|error| ToolError::io("canonicalize", path, error))?;
    require_under_root(workspace_root, path, &canonical)?;
    require_under_root(search_root, path, &canonical)?;
    if metadata.is_file() {
        files.push(canonical);
        return Ok(());
    }
    if !metadata.is_dir() {
        return Ok(());
    }

    let entries = fs::read_dir(path).map_err(|error| ToolError::io("list", path, error))?;
    for entry in entries {
        let entry = entry.map_err(|error| ToolError::io("list", path, error))?;
        collect_files(&entry.path(), workspace_root, search_root, files)?;
    }
    Ok(())
}

/// Replaces the FIRST occurrence of the pre-image; a caller that needs a
/// unique target must widen the pre-image with surrounding context. Pre-image
/// presence doubles as the staleness check: if the file no longer contains
/// it, the patch was computed against old contents and fails as a typed
/// conflict without writing.
struct AppliedPatch {
    result: BoundedResult,
    path: PathBuf,
    bytes_hash: String,
}

fn apply_patch(operation: &FsPatch, workspace_root: &Path) -> ToolResult<AppliedPatch> {
    if operation.preimage.is_empty() {
        return Err(ToolError::invalid_argument(
            "fs_patch preimage cannot be empty",
        ));
    }
    let mut source = open_locked_current(&operation.path)?;
    let resolved = fs::canonicalize(&operation.path)
        .map_err(|error| ToolError::io("canonicalize locked patch", &operation.path, error))?;
    require_under_root(workspace_root, &operation.path, &resolved)?;
    if resolved != operation.path {
        return Err(ToolError::Lifecycle {
            message: format!(
                "authorized patch path {} now resolves to {}; re-authorization required",
                operation.path.display(),
                resolved.display()
            ),
        });
    }
    let source_metadata = source
        .metadata()
        .map_err(|error| ToolError::io("inspect", &operation.path, error))?;
    let mut bytes = Vec::new();
    source
        .read_to_end(&mut bytes)
        .map_err(|error| ToolError::io("read", &operation.path, error))?;
    let contents = String::from_utf8(bytes).map_err(|error| ToolError::InvalidArgument {
        message: format!("{} is not UTF-8 text: {error}", operation.path.display()),
    })?;
    if !contents.contains(&operation.preimage) {
        return Err(ToolError::Conflict(FsPatchConflict {
            path: operation.path.clone(),
            expected_preimage: operation.preimage.clone(),
        }));
    }
    let patched = contents
        .replacen(&operation.preimage, &operation.replacement, 1)
        .into_bytes();
    let bytes_hash = format!("blake3:{}", blake3::hash(&patched).to_hex());
    let parent = operation
        .path
        .parent()
        .ok_or_else(|| ToolError::invalid_argument("fs_patch path has no parent directory"))?;
    let mut temporary = NamedTempFile::new_in(parent)
        .map_err(|error| ToolError::io("create patch temporary", parent, error))?;
    temporary
        .as_file()
        .set_permissions(source_metadata.permissions())
        .map_err(|error| ToolError::io("set patch permissions", temporary.path(), error))?;
    temporary
        .write_all(&patched)
        .map_err(|error| ToolError::io("write patch temporary", temporary.path(), error))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| ToolError::io("sync patch temporary", temporary.path(), error))?;
    temporary
        .as_file()
        .lock()
        .map_err(|error| ToolError::io("lock patch temporary", temporary.path(), error))?;
    let _persisted = temporary
        .persist(&operation.path)
        .map_err(|error| ToolError::io("replace patched file", &operation.path, error.error))?;

    Ok(AppliedPatch {
        result: BoundedResult {
            preview: format!("patched {}", operation.path.display()),
            truncated: false,
            artifact: None,
            cursor: None,
        },
        path: operation.path.clone(),
        bytes_hash,
    })
}

/// Opens and exclusively locks the inode currently named by `path`.
///
/// A writer may have opened the old inode before another writer atomically
/// replaced the path. Comparing handles after the lock detects that stale-open
/// race and retries against the replacement inode before reading any bytes.
fn open_locked_current(path: &Path) -> ToolResult<std::fs::File> {
    const MAX_STALE_OPEN_RETRIES: usize = 16;
    for _ in 0..MAX_STALE_OPEN_RETRIES {
        let source = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| ToolError::io("open for patch", path, error))?;
        source
            .lock()
            .map_err(|error| ToolError::io("lock for patch", path, error))?;
        let locked = Handle::from_file(
            source
                .try_clone()
                .map_err(|error| ToolError::io("clone locked patch", path, error))?,
        )
        .map_err(|error| ToolError::io("inspect locked patch", path, error))?;
        let current = Handle::from_path(path)
            .map_err(|error| ToolError::io("inspect current patch", path, error))?;
        if locked == current {
            return Ok(source);
        }
    }
    Err(ToolError::Runtime {
        message: format!(
            "patch target {} changed during {MAX_STALE_OPEN_RETRIES} lock attempts",
            path.display()
        ),
    })
}

/// Applies the result bound: a small result passes through untruncated; an
/// oversized one freezes its complete payload in the CAS and keeps a preview
/// cut back to the nearest UTF-8 character boundary.
async fn bounded<C>(
    contents: String,
    bounds: ResultBounds,
    cas: &mut C,
) -> ToolResult<BoundedResult>
where
    C: CasSink,
{
    if contents.len() <= bounds.max_preview_bytes {
        return Ok(BoundedResult {
            preview: contents,
            truncated: false,
            artifact: None,
            cursor: None,
        });
    }

    let mut preview_end = bounds.max_preview_bytes.min(contents.len());
    while preview_end > 0 && !contents.is_char_boundary(preview_end) {
        preview_end -= 1;
    }
    let artifact = cas.put(contents.as_bytes()).await?;
    Ok(BoundedResult {
        preview: contents[..preview_end].to_owned(),
        truncated: true,
        artifact: Some(artifact),
        cursor: None,
    })
}

fn join_lines(lines: Vec<String>) -> String {
    if lines.is_empty() {
        String::new()
    } else {
        let mut output = lines.join("\n");
        output.push('\n');
        output
    }
}

fn path_argument(path: &Path) -> ToolResult<&str> {
    path.to_str().ok_or_else(|| ToolError::InvalidArgument {
        message: format!("path is not valid UTF-8: {}", path.display()),
    })
}

#[derive(Debug, Clone, Copy)]
enum PathResolution {
    Existing,
    MissingLeafOk,
}

fn resolve_workspace_path(
    workspace_root: &Path,
    requested_path: &Path,
    resolution: PathResolution,
) -> ToolResult<PathBuf> {
    let candidate = if requested_path.is_absolute() {
        requested_path.to_path_buf()
    } else {
        workspace_root.join(requested_path)
    };
    let resolved = match fs::canonicalize(&candidate) {
        Ok(path) => path,
        Err(error)
            if matches!(resolution, PathResolution::MissingLeafOk)
                && error.kind() == std::io::ErrorKind::NotFound =>
        {
            if fs::symlink_metadata(&candidate)
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                return Err(ToolError::WorkspaceBoundary {
                    workspace_root: workspace_root.to_path_buf(),
                    requested_path: requested_path.to_path_buf(),
                    resolved_path: None,
                });
            }
            let parent = candidate
                .parent()
                .ok_or_else(|| ToolError::WorkspaceBoundary {
                    workspace_root: workspace_root.to_path_buf(),
                    requested_path: requested_path.to_path_buf(),
                    resolved_path: None,
                })?;
            let file_name = candidate
                .file_name()
                .ok_or_else(|| ToolError::WorkspaceBoundary {
                    workspace_root: workspace_root.to_path_buf(),
                    requested_path: requested_path.to_path_buf(),
                    resolved_path: None,
                })?;
            fs::canonicalize(parent)
                .map_err(|error| ToolError::io("canonicalize parent", parent, error))?
                .join(file_name)
        }
        Err(error) => {
            return Err(ToolError::io("canonicalize", &candidate, error));
        }
    };
    require_under_root(workspace_root, requested_path, &resolved)?;
    Ok(resolved)
}

fn require_under_root(
    workspace_root: &Path,
    requested_path: &Path,
    resolved_path: &Path,
) -> ToolResult<()> {
    if resolved_path.starts_with(workspace_root) {
        return Ok(());
    }
    Err(ToolError::WorkspaceBoundary {
        workspace_root: workspace_root.to_path_buf(),
        requested_path: requested_path.to_path_buf(),
        resolved_path: Some(resolved_path.to_path_buf()),
    })
}

async fn run_blocking<T>(
    operation: impl FnOnce() -> ToolResult<T> + Send + 'static,
) -> ToolResult<T>
where
    T: Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| ToolError::Runtime {
            message: format!("blocking filesystem worker failed: {error}"),
        })?
}
