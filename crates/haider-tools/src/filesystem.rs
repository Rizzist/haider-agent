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
//!   attributed to the caller's `(session, turn)`, for the verify gate.

use crate::broker::{EffectBroker, EffectOperation, JournalSink, PermissionPolicy};
use crate::ledger::{ChangeLedger, FsWriteRecord};
use crate::{FsPatchConflict, ToolError, ToolResult};
use async_trait::async_trait;
use haider_protocol::effect::EffectClass;
use haider_protocol::ids::{ArtifactRef, RunId, SessionId};
use haider_protocol::tool::BoundedResult;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

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
        let path = operation.path.clone();
        self.bounded_read(operation, policy, cas, bounds, move || read_utf8(&path))
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
        let path = operation.path.clone();
        self.bounded_read(operation, policy, cas, bounds, move || {
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
        let owned = operation.clone();
        self.bounded_read(operation, policy, cas, bounds, move || search_files(&owned))
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
        let intent = self.begin(operation, policy).await?;
        let owned_operation = operation.clone();
        let result = run_blocking(move || apply_patch(&owned_operation)).await;
        if result.is_ok() {
            // The write is real on disk at this point, so the ledger records
            // it even if the outcome append below fails — the verify gate's
            // evidence must never miss an applied write.
            ledger.record_fs_write(
                attribution.session.clone(),
                attribution.turn.clone(),
                FsWriteRecord {
                    effect: intent.effect.clone(),
                    paths: vec![operation.path.clone()],
                    summary: intent.summary.clone(),
                },
            );
        }
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

fn search_files(operation: &FsSearch) -> ToolResult<String> {
    if operation.query.is_empty() {
        return Err(ToolError::invalid_argument(
            "fs_search query cannot be empty",
        ));
    }
    let mut files = Vec::new();
    collect_files(&operation.root, &mut files)?;
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

fn collect_files(path: &Path, files: &mut Vec<PathBuf>) -> ToolResult<()> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| ToolError::io("inspect", path, error))?;
    // Symlinks are skipped entirely: following them risks cycles and reads
    // outside the search root.
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.is_file() {
        files.push(path.to_path_buf());
        return Ok(());
    }
    if !metadata.is_dir() {
        return Ok(());
    }

    let entries = fs::read_dir(path).map_err(|error| ToolError::io("list", path, error))?;
    for entry in entries {
        let entry = entry.map_err(|error| ToolError::io("list", path, error))?;
        collect_files(&entry.path(), files)?;
    }
    Ok(())
}

/// Replaces the FIRST occurrence of the pre-image; a caller that needs a
/// unique target must widen the pre-image with surrounding context. Pre-image
/// presence doubles as the staleness check: if the file no longer contains
/// it, the patch was computed against old contents and fails as a typed
/// conflict without writing.
fn apply_patch(operation: &FsPatch) -> ToolResult<BoundedResult> {
    if operation.preimage.is_empty() {
        return Err(ToolError::invalid_argument(
            "fs_patch preimage cannot be empty",
        ));
    }
    let contents = read_utf8(&operation.path)?;
    if !contents.contains(&operation.preimage) {
        return Err(ToolError::Conflict(FsPatchConflict {
            path: operation.path.clone(),
            expected_preimage: operation.preimage.clone(),
        }));
    }
    let patched = contents.replacen(&operation.preimage, &operation.replacement, 1);
    fs::write(&operation.path, patched.as_bytes())
        .map_err(|error| ToolError::io("write", &operation.path, error))?;
    Ok(BoundedResult {
        preview: format!("patched {}", operation.path.display()),
        truncated: false,
        artifact: None,
        cursor: None,
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
