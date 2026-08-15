//! Bounded filesystem tools executed through [`EffectBroker`].
//!
//! Owned invariants:
//! - Every tool runs inside the broker's begin/finish envelope, so the
//!   four-phase effect law covers reads and writes alike and no requested read
//!   or write runs before a journaled `Allow` + `Dispatched`.
//! - Read-class results are bounded: an oversized result keeps a UTF-8-safe
//!   preview and freezes the complete payload in the CAS via [`CasSink`].
//!   Search spools its complete match stream to a private temporary file while
//!   retaining at most 200 matches / 8 KiB for the prompt; glob retains the
//!   lexicographically first 500 paths and reports overflow honestly.
//! - File freshness is session-scoped and advances only with a durably
//!   journaled terminal outcome. File `fs_read`, `fs_write`, and `fs_edit`
//!   attach the exact BLAKE3 digest to that outcome. Existing-file writes and
//!   edits compare it against the locked current snapshot, returning typed
//!   unread or stale refusals before any replacement is prepared.
//! - Mutations record their post-mutation digest in the
//!   [`crate::ChangeLedger`],
//!   attributed to the caller's `(session, turn)`, for the verify gate. The
//!   mutation commit, ledger append, and terminal outcome decision are one
//!   indivisible blocking critical section. Write/edit commit by atomic
//!   replacement; path-copy completes in private staging before commit. A
//!   broker-owned finalizer always
//!   consumes that decision and journals its outcome, even if the calling task
//!   is cancelled: once the rename lands, the outcome is journaled as `Ok` or
//!   as `Failed` carrying the ledger error. [`EffectBroker::close`] drains all
//!   such finalizers, so no live-runtime shutdown path silently drops a
//!   successful write.
//! - Every caller-supplied path is resolved to a canonical path under the
//!   broker's canonical workspace root before it is digested or dispatched.
//!   Execution then converts that path back to a checked relative path and
//!   walks it component-by-component from the broker's retained root dirfd.
//!   Every open uses `O_NOFOLLOW`, so a post-authorization symlink swap is a
//!   typed refusal rather than an access outside the workspace.
//!   Structural mutations also re-resolve their held source/destination
//!   parents and leaf identities immediately before commit.
//! - Edit pre-image and final-verify bytes use a same-directory `clonefile`
//!   COW snapshot when Apple provides one. If cloning is unavailable or fails,
//!   the read degrades to a metadata-guarded best effort; that portable fallback
//!   cannot exclude an undetectably torn read from a non-cooperating writer.
//!   The derived bytes go to a same-directory temp opened through the parent
//!   dirfd and land through `renameat`. Both `fs_write` and `fs_edit` hold the
//!   target's advisory lock through rename, serializing broker-mediated writes
//!   to an existing target. Immediately before rename, the anchored path is
//!   checked against the locked inode and its original content hash, and the
//!   parent is freshly resolved from a root fd whose identity is still bound to
//!   the canonical workspace path. On Apple, namespace escapes are atomically
//!   refused; on fallback platforms, swaps already visible at the final parent
//!   recheck are refused. External replacements observed by the final identity
//!   check and preventable same-inode clobbers are typed refusals rather than
//!   silent overwrites. The remaining non-cooperating races and filesystem
//!   bounds are ledgered in `docs/OPTIMIZATIONS.md`.

use crate::broker::{EffectBroker, EffectOperation, PermissionPolicy};
use crate::ledger::{ChangeLedgerSink, FsWriteRecord};
use crate::{FsEditAnchorMismatch, ToolError, ToolResult};
use async_trait::async_trait;
use haider_protocol::effect::{EffectClass, FileFreshness, WorkspaceMutation};
use haider_protocol::ids::{ArtifactRef, RunId, SessionId};
use haider_protocol::tool::{BoundedResult, DispatchMode, ToolManifest};
use haider_platform::WorkspaceDirectory as OwnedFd;
#[cfg(unix)]
use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use serde_json::{Value, json};
use std::collections::{BinaryHeap, HashMap};
#[cfg(unix)]
use std::ffi::CStr;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
#[cfg(unix)]
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Port for storing the complete result when its prompt preview is truncated.
#[async_trait]
pub trait CasSink: Send {
    async fn put(&mut self, bytes: &[u8]) -> ToolResult<ArtifactRef>;
    /// Streams a staged file into CAS without rebuilding it as one buffer.
    async fn put_file(&mut self, path: &Path) -> ToolResult<ArtifactRef>;
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

pub fn fs_read_manifest() -> ToolManifest {
    ToolManifest {
        name: "fs_read".into(),
        description: "Read a bounded UTF-8 file slice or list a directory".into(),
        effects: vec![EffectClass::FsRead],
        dispatch: DispatchMode::Await,
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "minLength": 1},
                "offset": {"type": "integer", "minimum": 1},
                "limit": {"type": "integer", "minimum": 1}
            },
            "required": ["path"],
            "additionalProperties": false
        }),
    }
}

pub fn fs_glob_manifest() -> ToolManifest {
    ToolManifest {
        name: "fs_glob".into(),
        description: "List workspace files matching a bounded glob".into(),
        effects: vec![EffectClass::FsRead],
        dispatch: DispatchMode::Await,
        input_schema: json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "minLength": 1},
                "path": {"type": "string", "minLength": 1}
            },
            "required": ["pattern"],
            "additionalProperties": false
        }),
    }
}

pub fn fs_search_manifest() -> ToolManifest {
    ToolManifest {
        name: "fs_search".into(),
        description: "Search bounded workspace file contents".into(),
        effects: vec![EffectClass::FsRead],
        dispatch: DispatchMode::Await,
        input_schema: json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "minLength": 1},
                "path": {"type": "string", "minLength": 1},
                "glob": {"type": "string", "minLength": 1},
                "case": {"type": "string", "enum": ["sensitive", "insensitive", "smart"]},
                "mode": {"type": "string", "enum": ["literal", "simple"]},
                "query": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Legacy alias for pattern"
                },
                "root": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Legacy alias for path"
                }
            },
            "anyOf": [
                {"required": ["pattern"]},
                {"required": ["query"]}
            ],
            "additionalProperties": false
        }),
    }
}

pub fn fs_write_manifest() -> ToolManifest {
    ToolManifest {
        name: "fs_write".into(),
        description: "Create or replace one UTF-8 file, creating parent directories".into(),
        effects: vec![EffectClass::FsWrite],
        dispatch: DispatchMode::Await,
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "minLength": 1},
                "content": {"type": "string"}
            },
            "required": ["path", "content"],
            "additionalProperties": false
        }),
    }
}

pub fn fs_edit_manifest() -> ToolManifest {
    ToolManifest {
        name: "fs_edit".into(),
        description: "Atomically apply one or more anchored replacements to a fresh UTF-8 file"
            .into(),
        effects: vec![EffectClass::FsWrite],
        dispatch: DispatchMode::Await,
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "minLength": 1},
                "edits": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "old": {"type": "string", "minLength": 1},
                            "new": {"type": "string"},
                            "replace_all": {"type": "boolean"}
                        },
                        "required": ["old", "new"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["path", "edits"],
            "additionalProperties": false
        }),
    }
}

pub fn fs_path_manifest() -> ToolManifest {
    ToolManifest {
        name: "fs_path".into(),
        description: "Move, delete, or copy an existing workspace path".into(),
        effects: vec![EffectClass::FsWrite],
        dispatch: DispatchMode::Await,
        input_schema: json!({
            "type": "object",
            "properties": {
                "operation": {"type": "string", "enum": ["move", "delete", "copy"]},
                "source": {"type": "string", "minLength": 1},
                "destination": {"type": "string", "minLength": 1},
                "overwrite": {"type": "boolean"}
            },
            "required": ["operation", "source"],
            "additionalProperties": false
        }),
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
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

impl FsRead {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            offset: None,
            limit: None,
        }
    }

    /// Selects a one-based line range. Ranged reads are line-numbered; the
    /// default remains the byte-exact whole-file read used by freshness.
    pub fn with_line_range(mut self, offset: Option<usize>, limit: Option<usize>) -> Self {
        self.offset = offset;
        self.limit = limit;
        self
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
        Ok(json!({
            "limit": self.limit,
            "offset": self.offset,
            "path": path_argument(&self.path)?,
        }))
    }

    fn canonical_arguments(&self, workspace_root: &Path) -> ToolResult<Value> {
        let path = resolve_workspace_path(workspace_root, &self.path, PathResolution::Existing)?;
        Ok(json!({
            "limit": self.limit,
            "offset": self.offset,
            "path": path_argument(&path)?,
        }))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsSearch {
    pub root: PathBuf,
    pub query: String,
    pub glob: Option<String>,
    pub case_mode: FsCaseMode,
    pub mode: FsSearchMode,
}

impl FsSearch {
    pub fn new(root: impl Into<PathBuf>, query: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            query: query.into(),
            glob: None,
            case_mode: FsCaseMode::Sensitive,
            mode: FsSearchMode::Literal,
        }
    }

    pub fn with_glob(mut self, glob: impl Into<String>) -> Self {
        self.glob = Some(glob.into());
        self
    }

    pub fn with_case_mode(mut self, case_mode: FsCaseMode) -> Self {
        self.case_mode = case_mode;
        self
    }

    pub fn with_mode(mut self, mode: FsSearchMode) -> Self {
        self.mode = mode;
        self
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FsCaseMode {
    #[default]
    Sensitive,
    Insensitive,
    Smart,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FsSearchMode {
    #[default]
    Literal,
    /// Dependency-free wildcard matching: `*` matches any run and `?`
    /// matches one scalar. This is the brief's documented simple-pattern
    /// fallback; full regular expressions require a future dependency wave.
    Simple,
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
            "case": case_mode_argument(self.case_mode),
            "glob": self.glob,
            "mode": search_mode_argument(self.mode),
            "query": self.query,
            "root": path_argument(&self.root)?,
        }))
    }

    fn canonical_arguments(&self, workspace_root: &Path) -> ToolResult<Value> {
        let root = resolve_workspace_path(workspace_root, &self.root, PathResolution::Existing)?;
        Ok(json!({
            "case": case_mode_argument(self.case_mode),
            "glob": self.glob,
            "mode": search_mode_argument(self.mode),
            "query": self.query,
            "root": path_argument(&root)?,
        }))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsGlob {
    pub root: PathBuf,
    pub pattern: String,
}

impl FsGlob {
    pub fn new(root: impl Into<PathBuf>, pattern: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            pattern: pattern.into(),
        }
    }
}

impl EffectOperation for FsGlob {
    fn effect_class(&self) -> EffectClass {
        EffectClass::FsRead
    }

    fn summary(&self) -> String {
        format!("glob {} for {:?}", self.root.display(), self.pattern)
    }

    fn arguments(&self) -> ToolResult<Value> {
        Ok(json!({
            "pattern": self.pattern,
            "root": path_argument(&self.root)?,
        }))
    }

    fn canonical_arguments(&self, workspace_root: &Path) -> ToolResult<Value> {
        let root = resolve_workspace_path(workspace_root, &self.root, PathResolution::Existing)?;
        Ok(json!({
            "pattern": self.pattern,
            "root": path_argument(&root)?,
        }))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsEdit {
    pub path: PathBuf,
    pub edits: Vec<FsEditChange>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsEditChange {
    pub old: String,
    pub new: String,
    pub replace_all: bool,
}

impl FsEditChange {
    pub fn new(old: impl Into<String>, new: impl Into<String>) -> Self {
        Self {
            old: old.into(),
            new: new.into(),
            replace_all: false,
        }
    }

    pub fn replace_all(mut self, replace_all: bool) -> Self {
        self.replace_all = replace_all;
        self
    }
}

impl FsEdit {
    pub fn new(
        path: impl Into<PathBuf>,
        old_string: impl Into<String>,
        new_string: impl Into<String>,
    ) -> Self {
        Self {
            path: path.into(),
            edits: vec![FsEditChange::new(old_string, new_string)],
        }
    }

    pub fn replace_all(mut self, replace_all: bool) -> Self {
        if let Some(edit) = self.edits.first_mut() {
            edit.replace_all = replace_all;
        }
        self
    }

    pub fn many(path: impl Into<PathBuf>, edits: Vec<FsEditChange>) -> Self {
        Self {
            path: path.into(),
            edits,
        }
    }
}

impl EffectOperation for FsEdit {
    fn effect_class(&self) -> EffectClass {
        EffectClass::FsWrite
    }

    fn summary(&self) -> String {
        format!("edit {}", self.path.display())
    }

    fn arguments(&self) -> ToolResult<Value> {
        Ok(json!({
            "edits": self.edits.iter().map(|edit| json!({
                "new": edit.new,
                "old": edit.old,
                "replace_all": edit.replace_all,
            })).collect::<Vec<_>>(),
            "path": path_argument(&self.path)?,
        }))
    }

    fn canonical_arguments(&self, workspace_root: &Path) -> ToolResult<Value> {
        let path = resolve_workspace_path(workspace_root, &self.path, PathResolution::Existing)?;
        Ok(json!({
            "edits": self.edits.iter().map(|edit| json!({
                "new": edit.new,
                "old": edit.old,
                "replace_all": edit.replace_all,
            })).collect::<Vec<_>>(),
            "path": path_argument(&path)?,
        }))
    }

    fn approval_preview(&self) -> Vec<String> {
        let mut preview = vec![format!("Target: {}", self.path.display())];
        preview.extend(self.edits.iter().enumerate().map(|(index, edit)| {
            format!(
                "Edit {}: replace {} occurrence(s) of blake3:{} with {} UTF-8 bytes",
                index + 1,
                if edit.replace_all { "all" } else { "one" },
                blake3::hash(edit.old.as_bytes()).to_hex(),
                edit.new.len()
            )
        }));
        preview
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsWrite {
    pub path: PathBuf,
    pub content: String,
}

impl FsWrite {
    pub fn new(path: impl Into<PathBuf>, content: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            content: content.into(),
        }
    }
}

impl EffectOperation for FsWrite {
    fn effect_class(&self) -> EffectClass {
        EffectClass::FsWrite
    }

    fn summary(&self) -> String {
        format!("write {}", self.path.display())
    }

    fn arguments(&self) -> ToolResult<Value> {
        Ok(json!({
            "path": path_argument(&self.path)?,
            "content": self.content,
        }))
    }

    fn canonical_arguments(&self, workspace_root: &Path) -> ToolResult<Value> {
        let path =
            resolve_workspace_path(workspace_root, &self.path, PathResolution::MissingPathOk)?;
        Ok(json!({
            "path": path_argument(&path)?,
            "content": self.content,
        }))
    }

    fn approval_preview(&self) -> Vec<String> {
        vec![
            format!("Target: {}", self.path.display()),
            format!(
                "Content: {} UTF-8 bytes, {} lines, blake3:{}",
                self.content.len(),
                self.content.lines().count(),
                blake3::hash(self.content.as_bytes()).to_hex()
            ),
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsPathOperation {
    Move,
    Delete,
    Copy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsPath {
    pub operation: FsPathOperation,
    pub source: PathBuf,
    pub destination: Option<PathBuf>,
    pub overwrite: bool,
}

impl FsPath {
    pub fn new(operation: FsPathOperation, source: impl Into<PathBuf>) -> Self {
        Self {
            operation,
            source: source.into(),
            destination: None,
            overwrite: false,
        }
    }

    pub fn with_destination(mut self, destination: impl Into<PathBuf>) -> Self {
        self.destination = Some(destination.into());
        self
    }

    pub fn overwrite(mut self, overwrite: bool) -> Self {
        self.overwrite = overwrite;
        self
    }

    fn operation_name(&self) -> &'static str {
        match self.operation {
            FsPathOperation::Move => "move",
            FsPathOperation::Delete => "delete",
            FsPathOperation::Copy => "copy",
        }
    }

    fn validate_shape(&self) -> ToolResult<()> {
        match (self.operation, self.destination.as_ref()) {
            (FsPathOperation::Delete, None) => Ok(()),
            (FsPathOperation::Delete, Some(_)) => Err(ToolError::invalid_argument(
                "fs_path delete does not accept a destination",
            )),
            (FsPathOperation::Move | FsPathOperation::Copy, Some(_)) => Ok(()),
            (FsPathOperation::Move | FsPathOperation::Copy, None) => Err(
                ToolError::invalid_argument("fs_path move/copy requires a destination"),
            ),
        }
    }
}

impl EffectOperation for FsPath {
    fn effect_class(&self) -> EffectClass {
        EffectClass::FsWrite
    }

    fn summary(&self) -> String {
        match self.destination.as_ref() {
            Some(destination) => format!(
                "{} {} to {}",
                self.operation_name(),
                self.source.display(),
                destination.display()
            ),
            None => format!("delete {}", self.source.display()),
        }
    }

    fn arguments(&self) -> ToolResult<Value> {
        self.validate_shape()?;
        Ok(json!({
            "destination": self.destination.as_deref().map(path_argument).transpose()?,
            "operation": self.operation_name(),
            "overwrite": self.overwrite,
            "source": path_argument(&self.source)?,
        }))
    }

    fn canonical_arguments(&self, workspace_root: &Path) -> ToolResult<Value> {
        self.validate_shape()?;
        let source = resolve_workspace_path(
            workspace_root,
            &self.source,
            PathResolution::AnchoredExistingLeaf,
        )?;
        let destination = self
            .destination
            .as_deref()
            .map(|path| resolve_workspace_path(workspace_root, path, PathResolution::AnchoredLeaf))
            .transpose()?;
        Ok(json!({
            "destination": destination.as_deref().map(path_argument).transpose()?,
            "operation": self.operation_name(),
            "overwrite": self.overwrite,
            "source": path_argument(&source)?,
        }))
    }

    fn approval_preview(&self) -> Vec<String> {
        vec![self.summary(), format!("Overwrite: {}", self.overwrite)]
    }
}

impl EffectBroker {
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
        )?)
        .with_line_range(operation.offset, operation.limit);
        let relative = anchored_relative_path(self.workspace_root(), &operation.path)?;
        let display_path = operation.path.clone();
        let workspace_dir = self.duplicate_workspace_dir()?;
        let freshness_path = relative_path_argument(&relative)?.to_owned();
        let intent = self.begin(&operation, policy).await?;
        let offset = operation.offset;
        let limit = operation.limit;
        let read = run_blocking(move || {
            read_path_at(workspace_dir, &relative, &display_path, offset, limit)
        })
        .await;
        let (result, freshness) = match read {
            Ok(read) => {
                let result = bounded(read.contents, bounds, cas).await;
                let freshness = result.as_ref().ok().and_then(|_| {
                    read.digest.map(|digest| FileFreshness {
                        path: freshness_path,
                        digest,
                    })
                });
                (result, freshness)
            }
            Err(error) => (Err(error), None),
        };
        self.finish_with_freshness(&intent, result, freshness).await
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
        let requested_glob = operation.glob.clone();
        let mut operation = FsSearch::new(
            resolve_workspace_path(
                self.workspace_root(),
                &operation.root,
                PathResolution::Existing,
            )?,
            operation.query.clone(),
        )
        .with_case_mode(operation.case_mode)
        .with_mode(operation.mode);
        if let Some(glob) = requested_glob {
            operation = operation.with_glob(glob);
        }
        let owned = operation.clone();
        let relative = anchored_relative_path(self.workspace_root(), &operation.root)?;
        let workspace_dir = self.duplicate_workspace_dir()?;
        let intent = self.begin(&operation, policy).await?;
        let result = run_blocking(move || {
            search_files_at(workspace_dir, &relative, &owned, bounds.max_preview_bytes)
        })
        .await;
        let result = match result {
            Ok(matches) => bounded_search(matches, bounds, cas).await,
            Err(error) => Err(error),
        };
        self.finish(&intent, result).await
    }

    pub async fn fs_glob<C>(
        &mut self,
        operation: &FsGlob,
        policy: &PermissionPolicy,
        cas: &mut C,
        bounds: ResultBounds,
    ) -> ToolResult<BoundedResult>
    where
        C: CasSink,
    {
        let operation = FsGlob::new(
            resolve_workspace_path(
                self.workspace_root(),
                &operation.root,
                PathResolution::Existing,
            )?,
            operation.pattern.clone(),
        );
        let owned = operation.clone();
        let relative = anchored_relative_path(self.workspace_root(), &operation.root)?;
        let workspace_dir = self.duplicate_workspace_dir()?;
        let intent = self.begin(&operation, policy).await?;
        let result = run_blocking(move || glob_files_at(workspace_dir, &relative, &owned)).await;
        let result = match result {
            Ok(output) => {
                bounded_with_truncation(output.contents, output.truncated, bounds, cas).await
            }
            Err(error) => Err(error),
        };
        self.finish(&intent, result).await
    }

    pub async fn fs_write<L>(
        &mut self,
        operation: &FsWrite,
        policy: &PermissionPolicy,
        attribution: &TurnAttribution,
        ledger: &L,
    ) -> ToolResult<BoundedResult>
    where
        L: ChangeLedgerSink,
    {
        let operation = FsWrite::new(
            resolve_workspace_path(
                self.workspace_root(),
                &operation.path,
                PathResolution::MissingPathOk,
            )?,
            operation.content.clone(),
        );
        let intent = self.begin(&operation, policy).await?;
        let relative = anchored_relative_path(self.workspace_root(), &operation.path);
        let workspace_dir = self.duplicate_workspace_dir();
        let owned_operation = operation.clone();
        let critical_ledger = ledger.clone();
        let attribution = attribution.clone();
        let effect = intent.effect.clone();
        let summary = intent.summary.clone();
        let (relative, workspace_dir) = match (relative, workspace_dir) {
            (Ok(relative), Ok(workspace_dir)) => (relative, workspace_dir),
            (Err(error), _) | (_, Err(error)) => return self.finish(&intent, Err(error)).await,
        };
        let freshness_path = match relative_path_argument(&relative) {
            Ok(path) => path.to_owned(),
            Err(error) => return self.finish(&intent, Err(error)).await,
        };
        let expected_digest = self.freshness_digest(&relative);
        let worker = tokio::task::spawn_blocking(move || {
            apply_write_and_record(
                workspace_dir,
                &relative,
                &owned_operation,
                MutationRecordContext {
                    expected_digest: expected_digest.as_deref(),
                    ledger: &critical_ledger,
                    attribution,
                    effect,
                    summary,
                },
            )
        });
        let worker_abort = worker.abort_handle();
        let mut worker_cancel = WorkerCancelGuard::new(worker_abort);
        let finish = self.effect_finish(&intent);
        let (result_sender, result_receiver) = tokio::sync::oneshot::channel();
        let finalizer_id = self.register_finalizer(async move {
            let (result, freshness, workspace_mutation) = match worker.await {
                Ok(outcome) => outcome.into_result_with_freshness(freshness_path),
                Err(error) if error.is_cancelled() => return None,
                Err(error) => (
                    Err(ToolError::Runtime {
                        message: format!("blocking filesystem worker failed: {error}"),
                    }),
                    None,
                    None,
                ),
            };
            let result = finish
                .finish_with_workspace_mutation(result, freshness, workspace_mutation)
                .await;
            let error = result.as_ref().err().cloned();
            let _ = result_sender.send(result);
            error
        });
        match result_receiver.await {
            Ok(result) => {
                worker_cancel.disarm();
                self.observe_finalizer(finalizer_id);
                result
            }
            Err(error) => Err(ToolError::Runtime {
                message: format!("filesystem outcome finalizer failed: {error}"),
            }),
        }
    }

    pub async fn fs_edit<L>(
        &mut self,
        operation: &FsEdit,
        policy: &PermissionPolicy,
        attribution: &TurnAttribution,
        ledger: &L,
    ) -> ToolResult<BoundedResult>
    where
        L: ChangeLedgerSink,
    {
        let operation = FsEdit {
            path: resolve_workspace_path(
                self.workspace_root(),
                &operation.path,
                PathResolution::Existing,
            )?,
            edits: operation.edits.clone(),
        };
        let intent = self.begin(&operation, policy).await?;
        let relative = anchored_relative_path(self.workspace_root(), &operation.path);
        let workspace_dir = self.duplicate_workspace_dir();
        let owned_operation = operation.clone();
        let critical_ledger = ledger.clone();
        let attribution = attribution.clone();
        let effect = intent.effect.clone();
        let summary = intent.summary.clone();
        let (relative, workspace_dir) = match (relative, workspace_dir) {
            (Ok(relative), Ok(workspace_dir)) => (relative, workspace_dir),
            (Err(error), _) | (_, Err(error)) => return self.finish(&intent, Err(error)).await,
        };
        let freshness_path = match relative_path_argument(&relative) {
            Ok(path) => path.to_owned(),
            Err(error) => return self.finish(&intent, Err(error)).await,
        };
        let expected_digest = self.freshness_digest(&relative);
        let worker = tokio::task::spawn_blocking(move || {
            apply_edit_and_record(
                workspace_dir,
                &relative,
                &owned_operation,
                MutationRecordContext {
                    expected_digest: expected_digest.as_deref(),
                    ledger: &critical_ledger,
                    attribution,
                    effect,
                    summary,
                },
            )
        });
        let worker_abort = worker.abort_handle();
        let mut worker_cancel = WorkerCancelGuard::new(worker_abort);
        let finish = self.effect_finish(&intent);
        let (result_sender, result_receiver) = tokio::sync::oneshot::channel();
        let finalizer_id = self.register_finalizer(async move {
            let (result, freshness, workspace_mutation) = match worker.await {
                Ok(outcome) => outcome.into_result_with_freshness(freshness_path),
                Err(error) if error.is_cancelled() => return None,
                Err(error) => (
                    Err(ToolError::Runtime {
                        message: format!("blocking filesystem worker failed: {error}"),
                    }),
                    None,
                    None,
                ),
            };
            let result = finish
                .finish_with_workspace_mutation(result, freshness, workspace_mutation)
                .await;
            let error = result.as_ref().err().cloned();
            let _ = result_sender.send(result);
            error
        });
        match result_receiver.await {
            Ok(result) => {
                worker_cancel.disarm();
                self.observe_finalizer(finalizer_id);
                result
            }
            Err(error) => Err(ToolError::Runtime {
                message: format!("filesystem outcome finalizer failed: {error}"),
            }),
        }
    }

    pub async fn fs_path<L>(
        &mut self,
        operation: &FsPath,
        policy: &PermissionPolicy,
        attribution: &TurnAttribution,
        ledger: &L,
    ) -> ToolResult<BoundedResult>
    where
        L: ChangeLedgerSink,
    {
        operation.validate_shape()?;
        let source = resolve_workspace_path(
            self.workspace_root(),
            &operation.source,
            PathResolution::AnchoredExistingLeaf,
        )?;
        let destination = operation
            .destination
            .as_deref()
            .map(|path| {
                resolve_workspace_path(self.workspace_root(), path, PathResolution::AnchoredLeaf)
            })
            .transpose()?;
        let operation = FsPath {
            operation: operation.operation,
            source,
            destination,
            overwrite: operation.overwrite,
        };
        let intent = self.begin(&operation, policy).await?;
        let source_relative = anchored_relative_path(self.workspace_root(), &operation.source);
        let destination_relative = operation
            .destination
            .as_deref()
            .map(|path| anchored_relative_path(self.workspace_root(), path))
            .transpose();
        let workspace_dir = self.duplicate_workspace_dir();
        let owned_operation = operation.clone();
        let critical_ledger = ledger.clone();
        let attribution = attribution.clone();
        let effect = intent.effect.clone();
        let summary = intent.summary.clone();
        let (source_relative, destination_relative, workspace_dir) =
            match (source_relative, destination_relative, workspace_dir) {
                (Ok(source_relative), Ok(destination_relative), Ok(workspace_dir)) => {
                    (source_relative, destination_relative, workspace_dir)
                }
                (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => {
                    return self.finish(&intent, Err(error)).await;
                }
            };
        let worker = tokio::task::spawn_blocking(move || {
            apply_path_and_record(
                workspace_dir,
                &source_relative,
                destination_relative.as_deref(),
                &owned_operation,
                MutationRecordContext {
                    expected_digest: None,
                    ledger: &critical_ledger,
                    attribution,
                    effect,
                    summary,
                },
            )
        });
        // Tokio can abort a queued blocking task, but not one that has begun.
        // Caller cancellation therefore prevents an unstarted apply; once the
        // worker starts, the broker-owned finalizer must consume its decision.
        let worker_abort = worker.abort_handle();
        let mut worker_cancel = WorkerCancelGuard::new(worker_abort);
        let finish = self.effect_finish(&intent);
        let (result_sender, result_receiver) = tokio::sync::oneshot::channel();
        let finalizer_id = self.register_finalizer(async move {
            let (result, workspace_mutation) = match worker.await {
                Ok(outcome) => outcome.into_result(),
                Err(error) if error.is_cancelled() => return None,
                Err(error) => (
                    Err(ToolError::Runtime {
                        message: format!("blocking filesystem worker failed: {error}"),
                    }),
                    None,
                ),
            };
            let result = finish
                .finish_with_workspace_mutation(result, None, workspace_mutation)
                .await;
            let error = result.as_ref().err().cloned();
            let _ = result_sender.send(result);
            error
        });
        match result_receiver.await {
            Ok(result) => {
                worker_cancel.disarm();
                self.observe_finalizer(finalizer_id);
                result
            }
            Err(error) => Err(ToolError::Runtime {
                message: format!("filesystem outcome finalizer failed: {error}"),
            }),
        }
    }
}

struct WorkerCancelGuard {
    worker: tokio::task::AbortHandle,
    armed: bool,
}

impl WorkerCancelGuard {
    fn new(worker: tokio::task::AbortHandle) -> Self {
        Self {
            worker,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for WorkerCancelGuard {
    fn drop(&mut self) {
        if self.armed {
            self.worker.abort();
        }
    }
}

struct ReadPathOutput {
    contents: String,
    digest: Option<String>,
}

#[cfg(unix)]
fn read_path_at(
    workspace_dir: OwnedFd,
    relative: &Path,
    display_path: &Path,
    offset: Option<usize>,
    limit: Option<usize>,
) -> ToolResult<ReadPathOutput> {
    if offset == Some(0) {
        return Err(ToolError::invalid_argument(
            "fs_read offset must be one or greater",
        ));
    }
    if limit == Some(0) {
        return Err(ToolError::invalid_argument(
            "fs_read limit must be one or greater",
        ));
    }
    let target = open_target_at(
        workspace_dir,
        relative,
        OFlags::RDONLY,
        "open for read",
        display_path,
    )?;
    let metadata = rustix::fs::fstat(&target)
        .map_err(|error| ToolError::io("inspect read target", display_path, error))?;
    match FileType::from_raw_mode(metadata.st_mode) {
        FileType::RegularFile => {
            let contents = read_utf8_file(fs::File::from(target), display_path)?;
            let digest = mutation_digest(contents.as_bytes());
            let contents = if offset.is_some() || limit.is_some() {
                select_numbered_lines(&contents, offset.unwrap_or(1), limit)
            } else {
                contents
            };
            Ok(ReadPathOutput {
                contents,
                digest: Some(digest),
            })
        }
        FileType::Directory => Ok(ReadPathOutput {
            contents: list_directory_fd(target, display_path)?,
            digest: None,
        }),
        _ => Err(ToolError::invalid_argument(format!(
            "{} is neither a regular file nor a directory",
            display_path.display()
        ))),
    }
}

#[cfg(unix)]
fn read_utf8_file(mut file: fs::File, display_path: &Path) -> ToolResult<String> {
    let bytes =
        metadata_guarded_file_snapshot_with_reader(&mut file, display_path, |snapshot, buffer| {
            snapshot.read_at(buffer, 0)
        })?;
    String::from_utf8(bytes).map_err(|error| ToolError::InvalidArgument {
        message: format!("{} is not UTF-8 text: {error}", display_path.display()),
    })
}

#[cfg(windows)]
fn read_utf8_file(mut file: fs::File, display_path: &Path) -> ToolResult<String> {
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| ToolError::io("read", display_path, error))?;
    String::from_utf8(bytes).map_err(|error| ToolError::InvalidArgument {
        message: format!("{} is not UTF-8 text: {error}", display_path.display()),
    })
}

fn select_numbered_lines(contents: &str, offset: usize, limit: Option<usize>) -> String {
    let limit = limit.unwrap_or(usize::MAX);
    contents
        .split_inclusive('\n')
        .enumerate()
        .skip(offset - 1)
        .take(limit)
        .map(|(index, line)| format!("{}: {line}", index + 1))
        .collect()
}

#[cfg(unix)]
fn list_directory_fd(directory: OwnedFd, display_path: &Path) -> ToolResult<String> {
    let mut entries = rustix::fs::Dir::new(directory)
        .map_err(|error| ToolError::io("list", display_path, error))?;
    let mut listed = Vec::new();
    while let Some(entry) = entries.read() {
        let entry = entry.map_err(|error| ToolError::io("list", display_path, error))?;
        if is_dot_entry(entry.file_name()) {
            continue;
        }
        let mut name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type() == FileType::Directory {
            name.push('/');
        }
        listed.push(name);
    }
    listed.sort();
    Ok(join_lines(listed))
}

#[cfg(unix)]
fn search_files_at(
    workspace_dir: OwnedFd,
    relative: &Path,
    operation: &FsSearch,
    max_preview_bytes: usize,
) -> ToolResult<SearchOutput> {
    if operation.query.is_empty() {
        return Err(ToolError::invalid_argument(
            "fs_search query cannot be empty",
        ));
    }
    let directory = open_directory_at(workspace_dir, relative, "open for search", &operation.root)?;
    let mut matches = SearchCollector::new(max_preview_bytes)?;
    collect_search_matches_at(
        directory,
        Path::new(""),
        relative,
        &operation.root,
        operation,
        &mut matches,
    )?;
    Ok(matches.finish())
}

#[cfg(unix)]
fn collect_search_matches_at(
    directory: OwnedFd,
    relative: &Path,
    workspace_prefix: &Path,
    display_root: &Path,
    operation: &FsSearch,
    matches: &mut SearchCollector,
) -> ToolResult<()> {
    let mut entries = rustix::fs::Dir::read_from(&directory)
        .map_err(|error| ToolError::io("list", display_root.join(relative), error))?;
    let mut names = Vec::new();
    while let Some(entry) = entries.read() {
        let entry =
            entry.map_err(|error| ToolError::io("list", display_root.join(relative), error))?;
        let name = entry.file_name();
        if !is_dot_entry(name) {
            names.push(OsString::from_vec(name.to_bytes().to_vec()));
        }
    }
    names.sort();

    for name in names {
        let display_path = relative.join(&name);
        let entry_path = display_root.join(&display_path);
        let metadata = rustix::fs::statat(&directory, &name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|error| anchored_io_error("inspect", &entry_path, error))?;
        match FileType::from_raw_mode(metadata.st_mode) {
            FileType::Symlink => {}
            FileType::Directory => {
                let child = openat_nofollow(
                    &directory,
                    &name,
                    OFlags::RDONLY | OFlags::DIRECTORY,
                    "open search directory",
                    &entry_path,
                )?;
                collect_search_matches_at(
                    child,
                    &display_path,
                    workspace_prefix,
                    display_root,
                    operation,
                    matches,
                )?;
            }
            FileType::RegularFile => {
                let match_path = path_argument(&display_path)?;
                if operation
                    .glob
                    .as_deref()
                    .is_some_and(|glob| !glob_matches(glob, match_path))
                {
                    continue;
                }
                let file = openat_nofollow(
                    &directory,
                    &name,
                    OFlags::RDONLY | OFlags::NONBLOCK,
                    "open search file",
                    &entry_path,
                )?;
                let opened = rustix::fs::fstat(&file)
                    .map_err(|error| ToolError::io("inspect", &entry_path, error))?;
                if FileType::from_raw_mode(opened.st_mode) == FileType::RegularFile {
                    collect_file_matches(
                        file,
                        &workspace_prefix.join(&display_path),
                        &entry_path,
                        operation,
                        matches,
                    )?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(unix)]
fn collect_file_matches(
    file: OwnedFd,
    display_path: &Path,
    entry_path: &Path,
    operation: &FsSearch,
    matches: &mut SearchCollector,
) -> ToolResult<()> {
    let mut bytes = Vec::new();
    fs::File::from(file)
        .read_to_end(&mut bytes)
        .map_err(|error| ToolError::io("read", entry_path, error))?;
    let Ok(contents) = std::str::from_utf8(&bytes) else {
        return Ok(());
    };
    for (index, line) in contents.lines().enumerate() {
        if search_line_matches(line, operation) {
            matches.push(format!("{}:{}:{}", display_path.display(), index + 1, line))?;
        }
    }
    Ok(())
}

const SEARCH_PREVIEW_MATCHES: usize = 200;
const GLOB_ENTRY_LIMIT: usize = 500;

struct SearchOutput {
    preview: String,
    match_count: usize,
    total_bytes: usize,
    complete: tempfile::NamedTempFile,
}

struct SearchCollector {
    preview: String,
    match_count: usize,
    total_bytes: usize,
    max_preview_bytes: usize,
    preview_saturated: bool,
    complete: tempfile::NamedTempFile,
}

impl SearchCollector {
    fn new(max_preview_bytes: usize) -> ToolResult<Self> {
        let complete = tempfile::NamedTempFile::new()
            .map_err(|error| ToolError::io("create search result spool", "<search>", error))?;
        Ok(Self {
            preview: String::new(),
            match_count: 0,
            total_bytes: 0,
            max_preview_bytes,
            preview_saturated: false,
            complete,
        })
    }

    fn push(&mut self, line: String) -> ToolResult<()> {
        self.complete
            .write_all(line.as_bytes())
            .and_then(|()| self.complete.write_all(b"\n"))
            .map_err(|error| ToolError::io("write search result spool", "<search>", error))?;
        self.total_bytes = self
            .total_bytes
            .saturating_add(line.len())
            .saturating_add(1);
        if self.match_count < SEARCH_PREVIEW_MATCHES
            && self.preview.len() < self.max_preview_bytes
            && !self.preview_saturated
        {
            let remaining = self.max_preview_bytes - self.preview.len();
            self.preview.push_str(utf8_prefix(&line, remaining));
            if line.len() < remaining {
                self.preview.push('\n');
            } else {
                self.preview_saturated = true;
            }
        }
        self.match_count = self.match_count.saturating_add(1);
        Ok(())
    }

    fn finish(self) -> SearchOutput {
        SearchOutput {
            preview: self.preview,
            match_count: self.match_count,
            total_bytes: self.total_bytes,
            complete: self.complete,
        }
    }
}

struct CappedOutput {
    contents: String,
    truncated: bool,
}

struct GlobCollector {
    entries: BinaryHeap<String>,
    truncated: bool,
}

impl GlobCollector {
    fn new() -> Self {
        Self {
            entries: BinaryHeap::new(),
            truncated: false,
        }
    }

    fn push(&mut self, path: String) {
        if self.entries.len() < GLOB_ENTRY_LIMIT {
            self.entries.push(path);
            return;
        }
        self.truncated = true;
        if self
            .entries
            .peek()
            .is_some_and(|largest| path.as_str() < largest.as_str())
        {
            self.entries.pop();
            self.entries.push(path);
        }
    }
}

#[cfg(unix)]
fn glob_files_at(
    workspace_dir: OwnedFd,
    relative: &Path,
    operation: &FsGlob,
) -> ToolResult<CappedOutput> {
    if operation.pattern.is_empty() {
        return Err(ToolError::invalid_argument(
            "fs_glob pattern cannot be empty",
        ));
    }
    let directory = open_directory_at(workspace_dir, relative, "open for glob", &operation.root)?;
    let mut paths = GlobCollector::new();
    collect_glob_paths_at(
        directory,
        Path::new(""),
        relative,
        &operation.root,
        &operation.pattern,
        &mut paths,
    )?;
    let truncated = paths.truncated;
    Ok(CappedOutput {
        contents: join_lines(paths.entries.into_sorted_vec()),
        truncated,
    })
}

#[cfg(unix)]
fn collect_glob_paths_at(
    directory: OwnedFd,
    relative: &Path,
    workspace_prefix: &Path,
    display_root: &Path,
    pattern: &str,
    paths: &mut GlobCollector,
) -> ToolResult<()> {
    let mut entries = rustix::fs::Dir::read_from(&directory)
        .map_err(|error| ToolError::io("list", display_root.join(relative), error))?;
    let mut names = Vec::new();
    while let Some(entry) = entries.read() {
        let entry =
            entry.map_err(|error| ToolError::io("list", display_root.join(relative), error))?;
        if !is_dot_entry(entry.file_name()) {
            names.push(OsString::from_vec(entry.file_name().to_bytes().to_vec()));
        }
    }
    names.sort();

    for name in names {
        let child_relative = relative.join(&name);
        let entry_path = display_root.join(&child_relative);
        let metadata = rustix::fs::statat(&directory, &name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|error| anchored_io_error("inspect", &entry_path, error))?;
        match FileType::from_raw_mode(metadata.st_mode) {
            FileType::Symlink => {}
            FileType::Directory => {
                let child = openat_nofollow(
                    &directory,
                    &name,
                    OFlags::RDONLY | OFlags::DIRECTORY,
                    "open glob directory",
                    &entry_path,
                )?;
                collect_glob_paths_at(
                    child,
                    &child_relative,
                    workspace_prefix,
                    display_root,
                    pattern,
                    paths,
                )?;
            }
            FileType::RegularFile => {
                let match_path = path_argument(&child_relative)?;
                if glob_matches(pattern, match_path) {
                    paths.push(
                        relative_path_argument(&workspace_prefix.join(&child_relative))?.to_owned(),
                    );
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn search_line_matches(line: &str, operation: &FsSearch) -> bool {
    let case_sensitive = match operation.case_mode {
        FsCaseMode::Sensitive => true,
        FsCaseMode::Insensitive => false,
        FsCaseMode::Smart => operation.query.chars().any(char::is_uppercase),
    };
    let (line, query) = if case_sensitive {
        (line.to_owned(), operation.query.clone())
    } else {
        (line.to_lowercase(), operation.query.to_lowercase())
    };
    match operation.mode {
        FsSearchMode::Literal => line.contains(&query),
        FsSearchMode::Simple => wildcard_matches(&format!("*{query}*"), &line, false),
    }
}

fn glob_matches(pattern: &str, text: &str) -> bool {
    wildcard_matches(pattern, text, true)
}

fn wildcard_matches(pattern: &str, text: &str, slash_sensitive: bool) -> bool {
    let pattern = pattern.chars().collect::<Vec<_>>();
    let text = text.chars().collect::<Vec<_>>();
    fn matches_from(
        pattern: &[char],
        text: &[char],
        slash_sensitive: bool,
        pattern_index: usize,
        text_index: usize,
        memo: &mut HashMap<(usize, usize), bool>,
    ) -> bool {
        if let Some(result) = memo.get(&(pattern_index, text_index)) {
            return *result;
        }
        let result = match pattern.get(pattern_index) {
            None => text_index == text.len(),
            Some('*') => {
                let double = pattern.get(pattern_index + 1) == Some(&'*');
                let next_pattern = pattern_index + if double { 2 } else { 1 };
                (double
                    && pattern.get(next_pattern) == Some(&'/')
                    && matches_from(
                        pattern,
                        text,
                        slash_sensitive,
                        next_pattern + 1,
                        text_index,
                        memo,
                    ))
                    || matches_from(
                        pattern,
                        text,
                        slash_sensitive,
                        next_pattern,
                        text_index,
                        memo,
                    )
                    || text.get(text_index).is_some_and(|character| {
                        (double || !slash_sensitive || *character != '/')
                            && matches_from(
                                pattern,
                                text,
                                slash_sensitive,
                                pattern_index,
                                text_index + 1,
                                memo,
                            )
                    })
            }
            Some('?') => text.get(text_index).is_some_and(|character| {
                (!slash_sensitive || *character != '/')
                    && matches_from(
                        pattern,
                        text,
                        slash_sensitive,
                        pattern_index + 1,
                        text_index + 1,
                        memo,
                    )
            }),
            Some('\\') if pattern.get(pattern_index + 1).is_some() => {
                text.get(text_index) == pattern.get(pattern_index + 1)
                    && matches_from(
                        pattern,
                        text,
                        slash_sensitive,
                        pattern_index + 2,
                        text_index + 1,
                        memo,
                    )
            }
            Some(expected) => {
                text.get(text_index) == Some(expected)
                    && matches_from(
                        pattern,
                        text,
                        slash_sensitive,
                        pattern_index + 1,
                        text_index + 1,
                        memo,
                    )
            }
        };
        memo.insert((pattern_index, text_index), result);
        result
    }
    matches_from(&pattern, &text, slash_sensitive, 0, 0, &mut HashMap::new())
}

#[cfg(windows)]
fn read_path_at(
    workspace_dir: OwnedFd,
    relative: &Path,
    display_path: &Path,
    offset: Option<usize>,
    limit: Option<usize>,
) -> ToolResult<ReadPathOutput> {
    if offset == Some(0) {
        return Err(ToolError::invalid_argument(
            "fs_read offset must be one or greater",
        ));
    }
    if limit == Some(0) {
        return Err(ToolError::invalid_argument(
            "fs_read limit must be one or greater",
        ));
    }
    let target = windows_anchored_path(&workspace_dir, relative, display_path)?;
    let metadata = fs::symlink_metadata(&target)
        .map_err(|error| ToolError::io("inspect", display_path, error))?;
    if metadata.file_type().is_symlink() {
        return Err(ToolError::PathChanged {
            path: display_path.to_path_buf(),
            message: "symbolic links are refused".into(),
        });
    }
    if metadata.is_file() {
        let contents = read_utf8_file(
            fs::File::open(&target)
                .map_err(|error| ToolError::io("open for read", display_path, error))?,
            display_path,
        )?;
        let digest = format!("blake3:{}", blake3::hash(contents.as_bytes()).to_hex());
        let contents = offset.map_or(contents.clone(), |offset| {
            select_numbered_lines(&contents, offset, limit)
        });
        Ok(ReadPathOutput {
            contents,
            digest: Some(digest),
        })
    } else if metadata.is_dir() {
        let mut entries = fs::read_dir(&target)
            .map_err(|error| ToolError::io("list directory", display_path, error))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| ToolError::io("list directory", display_path, error))?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        let contents = entries
            .into_iter()
            .map(|entry| {
                let mut name = entry.file_name().to_string_lossy().into_owned();
                if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    name.push('/');
                }
                name
            })
            .collect::<Vec<_>>();
        Ok(ReadPathOutput {
            contents: join_lines(contents),
            digest: None,
        })
    } else {
        Err(ToolError::invalid_argument(format!(
            "fs_read path is not a regular file or directory: {}",
            display_path.display()
        )))
    }
}

#[cfg(windows)]
fn search_files_at(
    workspace_dir: OwnedFd,
    relative: &Path,
    operation: &FsSearch,
    max_preview_bytes: usize,
) -> ToolResult<SearchOutput> {
    if operation.query.is_empty() {
        return Err(ToolError::invalid_argument(
            "fs_search query cannot be empty",
        ));
    }
    let root = windows_anchored_path(&workspace_dir, relative, &operation.root)?;
    let mut matches = SearchCollector::new(max_preview_bytes)?;
    windows_walk_files(&root, &mut |path| {
        let relative = path.strip_prefix(&root).unwrap_or(path);
        if operation
            .glob
            .as_deref()
            .is_some_and(|glob| !glob_matches(glob, &relative.to_string_lossy().replace('\\', "/")))
        {
            return Ok(());
        }
        let bytes = fs::read(path).map_err(|error| ToolError::io("read", path, error))?;
        let Ok(contents) = std::str::from_utf8(&bytes) else {
            return Ok(());
        };
        for (index, line) in contents.lines().enumerate() {
            if search_line_matches(line, operation) {
                matches.push(format!("{}:{}:{}", path.display(), index + 1, line))?;
            }
        }
        Ok(())
    })?;
    Ok(matches.finish())
}

#[cfg(windows)]
fn glob_files_at(
    workspace_dir: OwnedFd,
    relative: &Path,
    operation: &FsGlob,
) -> ToolResult<CappedOutput> {
    if operation.pattern.is_empty() {
        return Err(ToolError::invalid_argument(
            "fs_glob pattern cannot be empty",
        ));
    }
    let root = windows_anchored_path(&workspace_dir, relative, &operation.root)?;
    let mut matches = GlobCollector::new();
    windows_walk_files(&root, &mut |path| {
        let relative = path.strip_prefix(&root).unwrap_or(path);
        let candidate = relative.to_string_lossy().replace('\\', "/");
        if glob_matches(&operation.pattern, &candidate) {
            matches.push(path.display().to_string());
        }
        Ok(())
    })?;
    Ok(matches.finish())
}

#[cfg(windows)]
fn windows_walk_files(
    root: &Path,
    visit: &mut impl FnMut(&Path) -> ToolResult<()>,
) -> ToolResult<()> {
    let metadata = fs::symlink_metadata(root)
        .map_err(|error| ToolError::io("inspect", root, error))?;
    if metadata.file_type().is_symlink() {
        return Err(ToolError::PathChanged {
            path: root.to_path_buf(),
            message: "symbolic links are refused".into(),
        });
    }
    if metadata.is_file() {
        return visit(root);
    }
    if !metadata.is_dir() {
        return Ok(());
    }
    let mut entries = fs::read_dir(root)
        .map_err(|error| ToolError::io("list", root, error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| ToolError::io("list", root, error))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let kind = entry
            .file_type()
            .map_err(|error| ToolError::io("inspect", &path, error))?;
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            windows_walk_files(&path, visit)?;
        } else if kind.is_file() {
            visit(&path)?;
        }
    }
    Ok(())
}

#[cfg(windows)]
fn windows_anchored_path(
    workspace_root: &Path,
    relative: &Path,
    display_path: &Path,
) -> ToolResult<PathBuf> {
    if relative.is_absolute()
        || relative.components().any(|component| {
            !matches!(
                component,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
    {
        return Err(ToolError::WorkspaceBoundary {
            workspace_root: workspace_root.to_path_buf(),
            requested_path: display_path.to_path_buf(),
            resolved_path: None,
        });
    }
    let candidate = workspace_root.join(relative);
    let canonical = fs::canonicalize(&candidate)
        .map_err(|error| ToolError::io("canonicalize", display_path, error))?;
    if !canonical.starts_with(workspace_root) {
        return Err(ToolError::WorkspaceBoundary {
            workspace_root: workspace_root.to_path_buf(),
            requested_path: display_path.to_path_buf(),
            resolved_path: Some(canonical),
        });
    }
    Ok(candidate)
}

struct AppliedMutation {
    result: BoundedResult,
    paths: Vec<PathBuf>,
    post_digest: String,
}

enum MutationWorkerOutcome {
    Applied {
        result: BoundedResult,
        effect: haider_protocol::ids::EffectId,
        post_digest: String,
    },
    ApplyFailed(ToolError),
    LedgerFailed {
        error: ToolError,
        written: bool,
        effect: haider_protocol::ids::EffectId,
        post_digest: String,
    },
}

impl MutationWorkerOutcome {
    fn into_result(self) -> (ToolResult<BoundedResult>, Option<WorkspaceMutation>) {
        match self {
            Self::Applied {
                result,
                effect,
                post_digest,
            } => (Ok(result), Some(workspace_mutation(effect, post_digest))),
            Self::ApplyFailed(error) => (Err(error), None),
            Self::LedgerFailed {
                error,
                written,
                effect,
                post_digest,
            } => {
                debug_assert!(written, "ledger failure must follow a successful rename");
                (Err(error), Some(workspace_mutation(effect, post_digest)))
            }
        }
    }

    fn into_result_with_freshness(
        self,
        relative_path: String,
    ) -> (
        ToolResult<BoundedResult>,
        Option<FileFreshness>,
        Option<WorkspaceMutation>,
    ) {
        match self {
            Self::Applied {
                result,
                effect,
                post_digest,
            } => {
                let mutation = workspace_mutation(effect, post_digest.clone());
                (
                    Ok(result),
                    Some(FileFreshness {
                        path: relative_path,
                        digest: post_digest,
                    }),
                    Some(mutation),
                )
            }
            Self::ApplyFailed(error) => (Err(error), None, None),
            Self::LedgerFailed {
                error,
                written,
                effect,
                post_digest,
            } => {
                debug_assert!(written, "ledger failure must follow a successful rename");
                let mutation = workspace_mutation(effect, post_digest.clone());
                (
                    Err(error),
                    Some(FileFreshness {
                        path: relative_path,
                        digest: post_digest,
                    }),
                    Some(mutation),
                )
            }
        }
    }
}

fn workspace_mutation(
    effect_id: haider_protocol::ids::EffectId,
    mutation_digest: String,
) -> WorkspaceMutation {
    WorkspaceMutation {
        effect_id,
        mutation_digest,
        workspace_revision: None,
        subject_digest: None,
    }
}

struct MutationRecordContext<'a, L> {
    expected_digest: Option<&'a str>,
    ledger: &'a L,
    attribution: TurnAttribution,
    effect: haider_protocol::ids::EffectId,
    summary: String,
}

#[cfg(windows)]
fn apply_write_and_record<L>(
    _workspace_dir: OwnedFd,
    _relative: &Path,
    _operation: &FsWrite,
    _context: MutationRecordContext<'_, L>,
) -> MutationWorkerOutcome
where
    L: ChangeLedgerSink,
{
    MutationWorkerOutcome::ApplyFailed(ToolError::Runtime {
        message: "fs_write is not yet available on Windows".into(),
    })
}

#[cfg(windows)]
fn apply_edit_and_record<L>(
    _workspace_dir: OwnedFd,
    _relative: &Path,
    _operation: &FsEdit,
    _context: MutationRecordContext<'_, L>,
) -> MutationWorkerOutcome
where
    L: ChangeLedgerSink,
{
    MutationWorkerOutcome::ApplyFailed(ToolError::Runtime {
        message: "fs_edit is not yet available on Windows".into(),
    })
}

#[cfg(windows)]
fn apply_path_and_record<L>(
    _workspace_dir: OwnedFd,
    _source_relative: &Path,
    _destination_relative: Option<&Path>,
    _operation: &FsPath,
    _context: MutationRecordContext<'_, L>,
) -> MutationWorkerOutcome
where
    L: ChangeLedgerSink,
{
    MutationWorkerOutcome::ApplyFailed(ToolError::Runtime {
        message: "fs_path mutations are not yet available on Windows".into(),
    })
}

#[cfg(unix)]
fn apply_write_and_record<L>(
    workspace_dir: OwnedFd,
    relative: &Path,
    operation: &FsWrite,
    context: MutationRecordContext<'_, L>,
) -> MutationWorkerOutcome
where
    L: ChangeLedgerSink,
{
    let applied = match apply_write_at(workspace_dir, relative, operation, context.expected_digest)
    {
        Ok(applied) => applied,
        Err(error) => return MutationWorkerOutcome::ApplyFailed(error),
    };
    let AppliedMutation {
        result,
        paths,
        post_digest,
    } = applied;
    let effect = context.effect.clone();
    match context.ledger.record_fs_write(
        context.attribution.session,
        context.attribution.turn,
        FsWriteRecord {
            effect: context.effect,
            paths,
            summary: context.summary,
            bytes_hash: post_digest.clone(),
        },
    ) {
        Ok(()) => MutationWorkerOutcome::Applied {
            result,
            effect,
            post_digest,
        },
        Err(error) => MutationWorkerOutcome::LedgerFailed {
            error,
            written: true,
            effect,
            post_digest,
        },
    }
}

#[cfg(unix)]
fn apply_write_at(
    workspace_dir: OwnedFd,
    relative: &Path,
    operation: &FsWrite,
    expected_digest: Option<&str>,
) -> ToolResult<AppliedMutation> {
    let traversal_root = rustix::io::dup(&workspace_dir)
        .map_err(|error| ToolError::io("duplicate workspace root", &operation.path, error))?;
    let (parent, leaf) = open_parent_creating_at(traversal_root, relative, &operation.path)?;
    // Keep the advisory-exclusive lock on the current inode alive through
    // rename when overwriting. `fs_edit` enters through the same lock helper,
    // so every cooperating Haider content mutation of an existing target
    // serializes.
    // A missing leaf is valid create semantics; any other lookup error stays
    // typed and no-follow.
    let mut source = match rustix::fs::statat(&parent, &leaf, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => {
            let (mut source, metadata) = open_locked_current_at(&parent, &leaf, &operation.path)?;
            let (source_bytes, _source_basis) =
                file_snapshot(&parent, &mut source, &operation.path)?.parts();
            let current_digest = mutation_digest(&source_bytes);
            let Some(expected_digest) = expected_digest else {
                return Err(ToolError::UnreadFile {
                    path: operation.path.clone(),
                });
            };
            if current_digest != expected_digest {
                return Err(ToolError::StaleRead {
                    path: operation.path.clone(),
                    recorded_digest: expected_digest.to_owned(),
                    current_digest,
                });
            }
            Some((source, metadata, blake3::hash(&source_bytes)))
        }
        Err(rustix::io::Errno::NOENT) => None,
        Err(error) => {
            return Err(anchored_io_error(
                "inspect write target",
                &operation.path,
                error,
            ));
        }
    };
    let bytes = operation.content.as_bytes();
    let post_digest = mutation_digest(bytes);
    let (temporary_name, temporary_fd) = create_patch_temporary(&parent, &operation.path)?;
    let mode = source
        .as_ref()
        .map_or(0o644, |(_, metadata, _)| metadata.st_mode);
    if let Err(error) = write_patch_temporary(temporary_fd, mode, bytes, &operation.path) {
        remove_temporary(&parent, &temporary_name);
        return Err(error);
    }
    if let Err(error) = require_unchanged_target(
        &parent,
        &leaf,
        source.as_ref().map(|(_, metadata, _)| metadata),
        &operation.path,
    ) {
        remove_temporary(&parent, &temporary_name);
        return Err(error);
    }
    let commit_parent =
        match revalidate_commit_parent(&workspace_dir, relative, &parent, &operation.path) {
            Ok(parent) => parent,
            Err(error) => {
                remove_temporary(&parent, &temporary_name);
                return Err(error);
            }
        };
    if let Some((source, _, source_hash)) = source.as_mut()
        && let Err(error) =
            require_unchanged_content(&parent, source, *source_hash, &operation.path)
    {
        remove_temporary(&parent, &temporary_name);
        return Err(error);
    }
    if let Err(error) = require_unchanged_target(
        &commit_parent,
        &leaf,
        source.as_ref().map(|(_, metadata, _)| metadata),
        &operation.path,
    ) {
        remove_temporary(&parent, &temporary_name);
        return Err(error);
    }
    if let Err(error) = replace_temporary_at_commit(
        &commit_parent,
        &temporary_name,
        &leaf,
        &operation.path,
        "replace written file",
    ) {
        remove_temporary(&parent, &temporary_name);
        return Err(error);
    }
    drop(source);
    Ok(AppliedMutation {
        result: BoundedResult {
            preview: format!(
                "wrote {} bytes to {}",
                bytes.len(),
                operation.path.display()
            ),
            truncated: false,
            artifact: None,
            cursor: None,
            status: haider_protocol::tool::ToolResultStatus::Completed,
            reason: None,
            presentation: None,
        },
        paths: vec![operation.path.clone()],
        post_digest,
    })
}

#[cfg(unix)]
fn apply_edit_and_record<L>(
    workspace_dir: OwnedFd,
    relative: &Path,
    operation: &FsEdit,
    context: MutationRecordContext<'_, L>,
) -> MutationWorkerOutcome
where
    L: ChangeLedgerSink,
{
    let applied = match apply_edit_at(workspace_dir, relative, operation, context.expected_digest) {
        Ok(applied) => applied,
        Err(error) => return MutationWorkerOutcome::ApplyFailed(error),
    };
    let AppliedMutation {
        result,
        paths,
        post_digest,
    } = applied;
    let effect = context.effect.clone();
    match context.ledger.record_fs_write(
        context.attribution.session,
        context.attribution.turn,
        FsWriteRecord {
            effect: context.effect,
            paths,
            summary: context.summary,
            bytes_hash: post_digest.clone(),
        },
    ) {
        Ok(()) => MutationWorkerOutcome::Applied {
            result,
            effect,
            post_digest,
        },
        Err(error) => MutationWorkerOutcome::LedgerFailed {
            error,
            written: true,
            effect,
            post_digest,
        },
    }
}

#[cfg(unix)]
fn apply_edit_at(
    workspace_dir: OwnedFd,
    relative: &Path,
    operation: &FsEdit,
    expected_digest: Option<&str>,
) -> ToolResult<AppliedMutation> {
    apply_edit_at_with_commit_hooks(
        workspace_dir,
        relative,
        operation,
        expected_digest,
        || {},
        || {},
    )
}

#[cfg(test)]
#[cfg(unix)]
fn apply_edit_at_before_replace(
    workspace_dir: OwnedFd,
    relative: &Path,
    operation: &FsEdit,
    expected_digest: Option<&str>,
    before_replace: impl FnOnce(),
) -> ToolResult<AppliedMutation> {
    apply_edit_at_with_commit_hooks(
        workspace_dir,
        relative,
        operation,
        expected_digest,
        before_replace,
        || {},
    )
}

#[cfg(unix)]
fn apply_edit_at_with_commit_hooks(
    workspace_dir: OwnedFd,
    relative: &Path,
    operation: &FsEdit,
    expected_digest: Option<&str>,
    before_replace: impl FnOnce(),
    before_commit: impl FnOnce(),
) -> ToolResult<AppliedMutation> {
    if operation.edits.is_empty() {
        return Err(ToolError::invalid_argument("fs_edit edits cannot be empty"));
    }
    if operation.edits.iter().any(|edit| edit.old.is_empty()) {
        return Err(ToolError::invalid_argument(
            "fs_edit old anchors cannot be empty",
        ));
    }
    let traversal_root = rustix::io::dup(&workspace_dir)
        .map_err(|error| ToolError::io("duplicate workspace root", &operation.path, error))?;
    let (parent, leaf) = open_parent_at(traversal_root, relative, &operation.path)?;
    let (mut source, source_metadata) = open_locked_current_at(&parent, &leaf, &operation.path)?;
    let (source_bytes, _source_basis) =
        file_snapshot(&parent, &mut source, &operation.path)?.parts();
    let current_digest = mutation_digest(&source_bytes);
    let Some(expected_digest) = expected_digest else {
        return Err(ToolError::UnreadFile {
            path: operation.path.clone(),
        });
    };
    if current_digest != expected_digest {
        return Err(ToolError::StaleRead {
            path: operation.path.clone(),
            recorded_digest: expected_digest.to_owned(),
            current_digest,
        });
    }
    let source_hash = blake3::hash(&source_bytes);
    let mut edited =
        String::from_utf8(source_bytes).map_err(|error| ToolError::InvalidArgument {
            message: format!("{} is not UTF-8 text: {error}", operation.path.display()),
        })?;
    let mut replacements = 0usize;
    for edit in &operation.edits {
        let matches = edited.match_indices(&edit.old).count();
        if (!edit.replace_all && matches != 1) || (edit.replace_all && matches == 0) {
            return Err(ToolError::EditAnchor(FsEditAnchorMismatch {
                path: operation.path.clone(),
                matches,
                replace_all: edit.replace_all,
            }));
        }
        edited = if edit.replace_all {
            edited.replace(&edit.old, &edit.new)
        } else {
            edited.replacen(&edit.old, &edit.new, 1)
        };
        replacements = replacements.saturating_add(if edit.replace_all { matches } else { 1 });
    }
    let bytes = edited.as_bytes();
    let post_digest = mutation_digest(bytes);
    let (temporary_name, temporary_fd) = create_patch_temporary(&parent, &operation.path)?;
    if let Err(error) = write_patch_temporary(
        temporary_fd,
        source_metadata.st_mode,
        bytes,
        &operation.path,
    ) {
        remove_temporary(&parent, &temporary_name);
        return Err(error);
    }
    before_replace();
    if let Err(error) =
        require_unchanged_target(&parent, &leaf, Some(&source_metadata), &operation.path)
    {
        remove_temporary(&parent, &temporary_name);
        return Err(error);
    }
    let commit_parent =
        match revalidate_commit_parent(&workspace_dir, relative, &parent, &operation.path) {
            Ok(parent) => parent,
            Err(error) => {
                remove_temporary(&parent, &temporary_name);
                return Err(error);
            }
        };
    if let Err(error) =
        require_unchanged_content(&parent, &mut source, source_hash, &operation.path)
    {
        remove_temporary(&parent, &temporary_name);
        return Err(error);
    }
    before_commit();
    if let Err(error) = require_unchanged_target(
        &commit_parent,
        &leaf,
        Some(&source_metadata),
        &operation.path,
    ) {
        remove_temporary(&parent, &temporary_name);
        return Err(error);
    }
    if let Err(error) = replace_temporary_at_commit(
        &commit_parent,
        &temporary_name,
        &leaf,
        &operation.path,
        "replace edited file",
    ) {
        remove_temporary(&parent, &temporary_name);
        return Err(error);
    }
    drop(source);
    Ok(AppliedMutation {
        result: BoundedResult {
            preview: format!(
                "edited {} ({} replacement{})",
                operation.path.display(),
                replacements,
                if replacements == 1 { "" } else { "s" }
            ),
            truncated: false,
            artifact: None,
            cursor: None,
            status: haider_protocol::tool::ToolResultStatus::Completed,
            reason: None,
            presentation: None,
        },
        paths: vec![operation.path.clone()],
        post_digest,
    })
}

#[cfg(unix)]
fn apply_path_and_record<L>(
    workspace_dir: OwnedFd,
    source_relative: &Path,
    destination_relative: Option<&Path>,
    operation: &FsPath,
    context: MutationRecordContext<'_, L>,
) -> MutationWorkerOutcome
where
    L: ChangeLedgerSink,
{
    let applied = match apply_path_at(
        workspace_dir,
        source_relative,
        destination_relative,
        operation,
    ) {
        Ok(applied) => applied,
        Err(error) => return MutationWorkerOutcome::ApplyFailed(error),
    };
    let AppliedMutation {
        result,
        paths,
        post_digest,
    } = applied;
    let effect = context.effect.clone();
    match context.ledger.record_fs_write(
        context.attribution.session,
        context.attribution.turn,
        FsWriteRecord {
            effect: context.effect,
            paths,
            summary: context.summary,
            bytes_hash: post_digest.clone(),
        },
    ) {
        Ok(()) => MutationWorkerOutcome::Applied {
            result,
            effect,
            post_digest,
        },
        Err(error) => MutationWorkerOutcome::LedgerFailed {
            error,
            written: true,
            effect,
            post_digest,
        },
    }
}

#[cfg(unix)]
fn apply_path_at(
    workspace_dir: OwnedFd,
    source_relative: &Path,
    destination_relative: Option<&Path>,
    operation: &FsPath,
) -> ToolResult<AppliedMutation> {
    apply_path_at_with_commit_hook(
        workspace_dir,
        source_relative,
        destination_relative,
        operation,
        || {},
    )
}

#[cfg(test)]
#[cfg(unix)]
fn apply_path_at_before_mutation(
    workspace_dir: OwnedFd,
    source_relative: &Path,
    destination_relative: Option<&Path>,
    operation: &FsPath,
    before_mutation: impl FnOnce(),
) -> ToolResult<AppliedMutation> {
    apply_path_at_with_commit_hook(
        workspace_dir,
        source_relative,
        destination_relative,
        operation,
        before_mutation,
    )
}

#[cfg(unix)]
fn apply_path_at_with_commit_hook(
    workspace_dir: OwnedFd,
    source_relative: &Path,
    destination_relative: Option<&Path>,
    operation: &FsPath,
    before_mutation: impl FnOnce(),
) -> ToolResult<AppliedMutation> {
    if source_relative.as_os_str().is_empty() {
        return Err(ToolError::invalid_argument(
            "fs_path refuses to mutate the workspace root",
        ));
    }
    let source_root = rustix::io::dup(&workspace_dir)
        .map_err(|error| ToolError::io("duplicate workspace root", &operation.source, error))?;
    let (source_parent, source_leaf) =
        open_parent_at(source_root, source_relative, &operation.source)?;
    let source_metadata =
        rustix::fs::statat(&source_parent, &source_leaf, AtFlags::SYMLINK_NOFOLLOW).map_err(
            |error| anchored_io_error("inspect fs_path source", &operation.source, error),
        )?;

    let mut structural = Vec::new();
    structural.extend_from_slice(operation.operation_name().as_bytes());
    structural.push(0);
    structural.extend_from_slice(relative_path_argument(source_relative)?.as_bytes());

    let mut before_mutation = Some(before_mutation);
    let (result, paths) = match operation.operation {
        FsPathOperation::Delete => {
            invoke_path_commit_hook(&mut before_mutation);
            let commit_source_parent = revalidate_commit_parent(
                &workspace_dir,
                source_relative,
                &source_parent,
                &operation.source,
            )?;
            require_unchanged_target(
                &commit_source_parent,
                &source_leaf,
                Some(&source_metadata),
                &operation.source,
            )?;
            remove_entry_at(&commit_source_parent, &source_leaf, &operation.source)?;
            (
                mutation_result(format!("deleted {}", operation.source.display())),
                vec![operation.source.clone()],
            )
        }
        FsPathOperation::Move | FsPathOperation::Copy => {
            let destination = operation.destination.as_ref().ok_or_else(|| {
                ToolError::invalid_argument("fs_path move/copy requires a destination")
            })?;
            let destination_relative = destination_relative.ok_or_else(|| {
                ToolError::invalid_argument("fs_path move/copy requires a destination")
            })?;
            if destination_relative.as_os_str().is_empty() {
                return Err(ToolError::invalid_argument(
                    "fs_path refuses to replace the workspace root",
                ));
            }
            if source_relative == destination_relative {
                return Err(ToolError::invalid_argument(
                    "fs_path source and destination must differ",
                ));
            }
            if FileType::from_raw_mode(source_metadata.st_mode) == FileType::Directory
                && destination_relative.starts_with(source_relative)
            {
                return Err(ToolError::invalid_argument(
                    "fs_path destination cannot be inside the source directory",
                ));
            }
            structural.push(0);
            structural.extend_from_slice(relative_path_argument(destination_relative)?.as_bytes());
            let destination_root = rustix::io::dup(&workspace_dir)
                .map_err(|error| ToolError::io("duplicate workspace root", destination, error))?;
            let (destination_parent, destination_leaf) =
                open_parent_at(destination_root, destination_relative, destination)?;
            let destination_metadata = match rustix::fs::statat(
                &destination_parent,
                &destination_leaf,
                AtFlags::SYMLINK_NOFOLLOW,
            ) {
                Ok(metadata) => Some(metadata),
                Err(rustix::io::Errno::NOENT) => None,
                Err(error) => {
                    return Err(anchored_io_error(
                        "inspect fs_path destination",
                        destination,
                        error,
                    ));
                }
            };
            if destination_metadata.is_some() && !operation.overwrite {
                return Err(ToolError::invalid_argument(format!(
                    "fs_path destination already exists: {}",
                    destination.display()
                )));
            }

            match operation.operation {
                FsPathOperation::Move => {
                    if destination_metadata.as_ref().is_some_and(|destination| {
                        destination.st_dev == source_metadata.st_dev
                            && destination.st_ino == source_metadata.st_ino
                    }) {
                        return Err(ToolError::invalid_argument(
                            "fs_path move source and destination identify the same path",
                        ));
                    }
                    invoke_path_commit_hook(&mut before_mutation);
                    let commit_source_parent = revalidate_commit_parent(
                        &workspace_dir,
                        source_relative,
                        &source_parent,
                        &operation.source,
                    )?;
                    let commit_destination_parent = revalidate_commit_parent(
                        &workspace_dir,
                        destination_relative,
                        &destination_parent,
                        destination,
                    )?;
                    require_unchanged_target(
                        &commit_source_parent,
                        &source_leaf,
                        Some(&source_metadata),
                        &operation.source,
                    )?;
                    require_unchanged_target(
                        &commit_destination_parent,
                        &destination_leaf,
                        destination_metadata.as_ref(),
                        destination,
                    )?;
                    if operation.overwrite {
                        rustix::fs::renameat(
                            &commit_source_parent,
                            &source_leaf,
                            &commit_destination_parent,
                            &destination_leaf,
                        )
                    } else {
                        rustix::fs::renameat_with(
                            &commit_source_parent,
                            &source_leaf,
                            &commit_destination_parent,
                            &destination_leaf,
                            rustix::fs::RenameFlags::NOREPLACE,
                        )
                    }
                    .map_err(|error| anchored_io_error("move path", destination, error))?;
                    (
                        mutation_result(format!(
                            "moved {} to {}",
                            operation.source.display(),
                            destination.display()
                        )),
                        vec![operation.source.clone(), destination.clone()],
                    )
                }
                FsPathOperation::Copy => {
                    invoke_path_commit_hook(&mut before_mutation);
                    let copy_source_parent = revalidate_commit_parent(
                        &workspace_dir,
                        source_relative,
                        &source_parent,
                        &operation.source,
                    )?;
                    let copy_destination_parent = revalidate_commit_parent(
                        &workspace_dir,
                        destination_relative,
                        &destination_parent,
                        destination,
                    )?;
                    require_unchanged_target(
                        &copy_source_parent,
                        &source_leaf,
                        Some(&source_metadata),
                        &operation.source,
                    )?;
                    require_unchanged_target(
                        &copy_destination_parent,
                        &destination_leaf,
                        destination_metadata.as_ref(),
                        destination,
                    )?;
                    let (staging_name, staging_directory) =
                        create_path_staging_directory(&copy_destination_parent, destination)?;
                    let staging_leaf = OsStr::new("entry");
                    if let Err(error) = copy_entry_at(
                        &copy_source_parent,
                        &source_leaf,
                        &staging_directory,
                        staging_leaf,
                        &operation.source,
                        destination,
                        &mut structural,
                    ) {
                        remove_path_staging(&copy_destination_parent, &staging_name, destination);
                        return Err(error);
                    }
                    let commit_source_parent = match revalidate_commit_parent(
                        &workspace_dir,
                        source_relative,
                        &copy_source_parent,
                        &operation.source,
                    ) {
                        Ok(parent) => parent,
                        Err(error) => {
                            remove_path_staging(
                                &copy_destination_parent,
                                &staging_name,
                                destination,
                            );
                            return Err(error);
                        }
                    };
                    let commit_destination_parent = match revalidate_commit_parent(
                        &workspace_dir,
                        destination_relative,
                        &copy_destination_parent,
                        destination,
                    ) {
                        Ok(parent) => parent,
                        Err(error) => {
                            remove_path_staging(
                                &copy_destination_parent,
                                &staging_name,
                                destination,
                            );
                            return Err(error);
                        }
                    };
                    let commit_checks = require_unchanged_target(
                        &commit_source_parent,
                        &source_leaf,
                        Some(&source_metadata),
                        &operation.source,
                    )
                    .and_then(|()| {
                        require_unchanged_target(
                            &commit_destination_parent,
                            &destination_leaf,
                            destination_metadata.as_ref(),
                            destination,
                        )
                    });
                    if let Err(error) = commit_checks {
                        remove_path_staging(&copy_destination_parent, &staging_name, destination);
                        return Err(error);
                    }
                    if let Err(failure) = commit_staged_copy(
                        &staging_directory,
                        staging_leaf,
                        &commit_destination_parent,
                        &destination_leaf,
                        destination_metadata.is_some(),
                        destination,
                    ) {
                        let (error, cleanup_safe) = failure.parts();
                        if cleanup_safe {
                            remove_path_staging(
                                &copy_destination_parent,
                                &staging_name,
                                destination,
                            );
                        }
                        return Err(error);
                    }
                    remove_path_staging(&copy_destination_parent, &staging_name, destination);
                    (
                        mutation_result(format!(
                            "copied {} to {}",
                            operation.source.display(),
                            destination.display()
                        )),
                        vec![destination.clone()],
                    )
                }
                FsPathOperation::Delete => unreachable!("covered above"),
            }
        }
    };

    Ok(AppliedMutation {
        result,
        paths,
        post_digest: mutation_digest(&structural),
    })
}

#[cfg(unix)]
fn invoke_path_commit_hook(hook: &mut Option<impl FnOnce()>) {
    if let Some(hook) = hook.take() {
        hook();
    }
}

fn mutation_result(preview: String) -> BoundedResult {
    BoundedResult {
        preview,
        truncated: false,
        artifact: None,
        cursor: None,
        status: haider_protocol::tool::ToolResultStatus::Completed,
        reason: None,
        presentation: None,
    }
}

#[cfg(unix)]
fn create_path_staging_directory(
    parent: &OwnedFd,
    display_path: &Path,
) -> ToolResult<(OsString, OwnedFd)> {
    static NEXT_STAGING: AtomicU64 = AtomicU64::new(0);
    const MAX_NAME_RETRIES: usize = 16;
    for _ in 0..MAX_NAME_RETRIES {
        let sequence = NEXT_STAGING.fetch_add(1, Ordering::Relaxed);
        let name = OsString::from(format!(
            ".haider-path-{}-{sequence}.tmp",
            std::process::id()
        ));
        match rustix::fs::mkdirat(parent, &name, Mode::from_raw_mode(0o700)) {
            Ok(()) => {
                let directory = match openat_nofollow(
                    parent,
                    &name,
                    OFlags::RDONLY | OFlags::DIRECTORY,
                    "open path staging directory",
                    display_path,
                ) {
                    Ok(directory) => directory,
                    Err(error) => {
                        let _ = rustix::fs::unlinkat(parent, &name, AtFlags::REMOVEDIR);
                        return Err(error);
                    }
                };
                return Ok((name, directory));
            }
            Err(rustix::io::Errno::EXIST) => {}
            Err(error) => {
                return Err(ToolError::io(
                    "create path staging directory",
                    display_path,
                    error,
                ));
            }
        }
    }
    Err(ToolError::Runtime {
        message: format!(
            "could not allocate a unique path staging directory for {}",
            display_path.display()
        ),
    })
}

enum StagedCopyCommitFailure {
    CleanupSafe(ToolError),
    PreserveStaging(ToolError),
}

impl StagedCopyCommitFailure {
    fn parts(self) -> (ToolError, bool) {
        match self {
            Self::CleanupSafe(error) => (error, true),
            Self::PreserveStaging(error) => (error, false),
        }
    }
}

#[cfg(unix)]
fn commit_staged_copy(
    staging_directory: &OwnedFd,
    staging_leaf: &OsStr,
    destination_parent: &OwnedFd,
    destination_leaf: &OsStr,
    destination_exists: bool,
    destination_path: &Path,
) -> Result<(), StagedCopyCommitFailure> {
    let previous_leaf = OsStr::new("previous");
    if destination_exists {
        rustix::fs::renameat_with(
            destination_parent,
            destination_leaf,
            staging_directory,
            previous_leaf,
            rustix::fs::RenameFlags::NOREPLACE,
        )
        .map_err(|error| {
            StagedCopyCommitFailure::CleanupSafe(anchored_io_error(
                "stage previous copy destination",
                destination_path,
                error,
            ))
        })?;
    }

    if let Err(error) = rustix::fs::renameat_with(
        staging_directory,
        staging_leaf,
        destination_parent,
        destination_leaf,
        rustix::fs::RenameFlags::NOREPLACE,
    ) {
        if destination_exists
            && let Err(rollback_error) = rustix::fs::renameat_with(
                staging_directory,
                previous_leaf,
                destination_parent,
                destination_leaf,
                rustix::fs::RenameFlags::NOREPLACE,
            )
        {
            return Err(StagedCopyCommitFailure::PreserveStaging(
                ToolError::PathChanged {
                    path: destination_path.to_path_buf(),
                    message: format!(
                        "copy commit failed ({error}) and restoring the prior destination failed ({rollback_error})"
                    ),
                },
            ));
        }
        return Err(StagedCopyCommitFailure::CleanupSafe(anchored_io_error(
            "commit staged copy",
            destination_path,
            error,
        )));
    }

    if destination_exists {
        // The new destination is committed. Cleanup is deliberately best
        // effort so an inability to remove the private backup cannot report a
        // false pre-commit failure after the visible mutation succeeded.
        let _ = remove_entry_at(staging_directory, previous_leaf, destination_path);
    }
    Ok(())
}

#[cfg(unix)]
fn remove_path_staging(parent: &OwnedFd, name: &OsStr, display_path: &Path) {
    let _ = remove_entry_at(parent, name, display_path);
}

#[cfg(unix)]
fn remove_entry_at(parent: &OwnedFd, leaf: &OsStr, display_path: &Path) -> ToolResult<()> {
    let metadata = rustix::fs::statat(parent, leaf, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|error| anchored_io_error("inspect delete target", display_path, error))?;
    if FileType::from_raw_mode(metadata.st_mode) != FileType::Directory {
        return rustix::fs::unlinkat(parent, leaf, AtFlags::empty())
            .map_err(|error| anchored_io_error("delete path", display_path, error));
    }

    let directory = openat_nofollow(
        parent,
        leaf,
        OFlags::RDONLY | OFlags::DIRECTORY,
        "open delete directory",
        display_path,
    )?;
    let mut entries = rustix::fs::Dir::read_from(&directory)
        .map_err(|error| ToolError::io("list delete directory", display_path, error))?;
    let mut names = Vec::new();
    while let Some(entry) = entries.read() {
        let entry =
            entry.map_err(|error| ToolError::io("list delete directory", display_path, error))?;
        if !is_dot_entry(entry.file_name()) {
            names.push(OsString::from_vec(entry.file_name().to_bytes().to_vec()));
        }
    }
    names.sort();
    for name in names {
        remove_entry_at(&directory, &name, &display_path.join(&name))?;
    }
    rustix::fs::unlinkat(parent, leaf, AtFlags::REMOVEDIR)
        .map_err(|error| anchored_io_error("delete directory", display_path, error))
}

#[cfg(unix)]
fn copy_entry_at(
    source_parent: &OwnedFd,
    source_leaf: &OsStr,
    destination_parent: &OwnedFd,
    destination_leaf: &OsStr,
    source_path: &Path,
    destination_path: &Path,
    structural: &mut Vec<u8>,
) -> ToolResult<()> {
    let metadata = rustix::fs::statat(source_parent, source_leaf, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|error| anchored_io_error("inspect copy source", source_path, error))?;
    match FileType::from_raw_mode(metadata.st_mode) {
        FileType::RegularFile => {
            let source = openat_nofollow(
                source_parent,
                source_leaf,
                OFlags::RDONLY,
                "open copy source",
                source_path,
            )?;
            let mut source = fs::File::from(source);
            let bytes = metadata_guarded_file_snapshot_with_reader(
                &mut source,
                source_path,
                |snapshot, buffer| snapshot.read_at(buffer, 0),
            )?;
            let destination = rustix::fs::openat(
                destination_parent,
                destination_leaf,
                OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            )
            .map_err(|error| {
                anchored_io_error("create copy destination", destination_path, error)
            })?;
            if let Err(error) =
                write_patch_temporary(destination, metadata.st_mode, &bytes, destination_path)
            {
                let _ =
                    rustix::fs::unlinkat(destination_parent, destination_leaf, AtFlags::empty());
                return Err(error);
            }
            structural.extend_from_slice(b"\0file\0");
            structural.extend_from_slice(destination_leaf.as_encoded_bytes());
            structural.push(0);
            structural.extend_from_slice(&bytes);
            Ok(())
        }
        FileType::Directory => {
            rustix::fs::mkdirat(
                destination_parent,
                destination_leaf,
                Mode::from_raw_mode(metadata.st_mode),
            )
            .map_err(|error| anchored_io_error("create copy directory", destination_path, error))?;
            let source_directory = openat_nofollow(
                source_parent,
                source_leaf,
                OFlags::RDONLY | OFlags::DIRECTORY,
                "open copy source directory",
                source_path,
            )?;
            let destination_directory = openat_nofollow(
                destination_parent,
                destination_leaf,
                OFlags::RDONLY | OFlags::DIRECTORY,
                "open copy destination directory",
                destination_path,
            )?;
            let mut entries = rustix::fs::Dir::read_from(&source_directory)
                .map_err(|error| ToolError::io("list copy source", source_path, error))?;
            let mut names = Vec::new();
            while let Some(entry) = entries.read() {
                let entry =
                    entry.map_err(|error| ToolError::io("list copy source", source_path, error))?;
                if !is_dot_entry(entry.file_name()) {
                    names.push(OsString::from_vec(entry.file_name().to_bytes().to_vec()));
                }
            }
            names.sort();
            structural.extend_from_slice(b"\0directory\0");
            structural.extend_from_slice(destination_leaf.as_encoded_bytes());
            for name in names {
                copy_entry_at(
                    &source_directory,
                    &name,
                    &destination_directory,
                    &name,
                    &source_path.join(&name),
                    &destination_path.join(&name),
                    structural,
                )?;
            }
            Ok(())
        }
        FileType::Symlink => Err(ToolError::PathChanged {
            path: source_path.to_path_buf(),
            message: "copying symbolic links is refused".into(),
        }),
        _ => Err(ToolError::invalid_argument(format!(
            "fs_path cannot copy special path {}",
            source_path.display()
        ))),
    }
}

#[cfg(all(unix, target_vendor = "apple"))]
fn replace_temporary_at_commit(
    parent: &OwnedFd,
    temporary_name: &OsStr,
    _leaf: &OsStr,
    display_path: &Path,
    operation: &'static str,
) -> ToolResult<()> {
    // sys/stdio.h: resolve both rename paths without following *any* symlink.
    // Passing the authorized absolute destination makes path resolution and
    // rename one kernel operation, so a parent moved after the userspace
    // recheck cannot redirect the commit through an outside-pointing symlink.
    const RENAME_NOFOLLOW_ANY: u32 = 0x0000_0010;
    let flags = rustix::fs::RenameFlags::from_bits_retain(RENAME_NOFOLLOW_ANY);
    rustix::fs::renameat_with(parent, temporary_name, rustix::fs::CWD, display_path, flags)
        .map_err(|error| anchored_io_error(operation, display_path, error))
}

#[cfg(all(unix, not(target_vendor = "apple")))]
fn replace_temporary_at_commit(
    parent: &OwnedFd,
    temporary_name: &OsStr,
    leaf: &OsStr,
    display_path: &Path,
    operation: &'static str,
) -> ToolResult<()> {
    rustix::fs::renameat(parent, temporary_name, parent, leaf)
        .map_err(|error| anchored_io_error(operation, display_path, error))
}

#[cfg(unix)]
fn require_unchanged_content(
    parent: &OwnedFd,
    source: &mut fs::File,
    expected: blake3::Hash,
    display_path: &Path,
) -> ToolResult<()> {
    let (bytes, _basis) = file_snapshot(parent, source, display_path)?.parts();
    if blake3::hash(&bytes) == expected {
        return Ok(());
    }
    Err(ToolError::PathChanged {
        path: display_path.to_path_buf(),
        message: "target content changed before atomic replace".into(),
    })
}

const SNAPSHOT_ATTEMPTS: usize = 4;
const MAX_SINGLE_READ_SNAPSHOT_BYTES: usize = i32::MAX as usize - 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SnapshotBasis {
    CowClone,
    MetadataGuardedFallback,
}

#[derive(Debug)]
struct FileSnapshot {
    bytes: Vec<u8>,
    basis: SnapshotBasis,
}

impl FileSnapshot {
    fn parts(self) -> (Vec<u8>, SnapshotBasis) {
        (self.bytes, self.basis)
    }
}

#[cfg(unix)]
fn file_snapshot(
    parent: &OwnedFd,
    source: &mut fs::File,
    display_path: &Path,
) -> ToolResult<FileSnapshot> {
    file_snapshot_with_reader(
        parent,
        source,
        display_path,
        try_clone_file_at,
        |snapshot, buffer| snapshot.read_at(buffer, 0),
    )
}

#[cfg(unix)]
fn file_snapshot_with_reader(
    parent: &OwnedFd,
    source: &mut fs::File,
    display_path: &Path,
    clone_source: impl FnOnce(&OwnedFd, &fs::File) -> Option<fs::File>,
    mut read_once: impl FnMut(&fs::File, &mut [u8]) -> std::io::Result<usize>,
) -> ToolResult<FileSnapshot> {
    if let Some(mut clone) = clone_source(parent, source) {
        return metadata_guarded_file_snapshot_with_reader(
            &mut clone,
            display_path,
            &mut read_once,
        )
        .map(|bytes| FileSnapshot {
            bytes,
            basis: SnapshotBasis::CowClone,
        });
    }
    metadata_guarded_file_snapshot_with_reader(source, display_path, read_once).map(|bytes| {
        FileSnapshot {
            bytes,
            basis: SnapshotBasis::MetadataGuardedFallback,
        }
    })
}

/// Takes one positional read bracketed by content-changing metadata checks.
/// The one-read shape avoids the old multi-chunk stream tear, and changing
/// identity, size, or timestamps trigger a bounded retry. This remains
/// best-effort for a non-cooperating writer: MAP_SHARED writes can leave those
/// fields unchanged, and ordinary writes may evade coarse timestamps.
#[cfg(unix)]
fn metadata_guarded_file_snapshot_with_reader(
    source: &mut fs::File,
    display_path: &Path,
    mut read_once: impl FnMut(&fs::File, &mut [u8]) -> std::io::Result<usize>,
) -> ToolResult<Vec<u8>> {
    for _ in 0..SNAPSHOT_ATTEMPTS {
        let before = rustix::fs::fstat(&*source)
            .map_err(|error| ToolError::io("inspect patch snapshot", display_path, error))?;
        let expected_len = usize::try_from(before.st_size).map_err(|_| ToolError::Runtime {
            message: format!(
                "patch target {} is too large to snapshot",
                display_path.display()
            ),
        })?;
        if expected_len > MAX_SINGLE_READ_SNAPSHOT_BYTES {
            return Err(ToolError::Runtime {
                message: format!(
                    "patch target {} exceeds the {MAX_SINGLE_READ_SNAPSHOT_BYTES}-byte \
                     single-read snapshot limit",
                    display_path.display()
                ),
            });
        }
        let buffer_len = expected_len + 1;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(buffer_len)
            .map_err(|error| ToolError::Runtime {
                message: format!(
                    "could not reserve a {buffer_len}-byte snapshot for {}: {error}",
                    display_path.display()
                ),
            })?;
        bytes.resize(buffer_len, 0);
        let bytes_read = read_once(source, &mut bytes)
            .map_err(|error| ToolError::io("read patch snapshot", display_path, error))?;
        let after = rustix::fs::fstat(&*source)
            .map_err(|error| ToolError::io("reinspect patch snapshot", display_path, error))?;
        if bytes_read == expected_len && snapshot_metadata_matches(&before, &after) {
            bytes.truncate(expected_len);
            return Ok(bytes);
        }
    }
    Err(ToolError::PathChanged {
        path: display_path.to_path_buf(),
        message: format!(
            "target content did not yield a stable snapshot after \
             {SNAPSHOT_ATTEMPTS} attempts"
        ),
    })
}

#[cfg(target_vendor = "apple")]
fn try_clone_file_at(parent: &OwnedFd, source: &fs::File) -> Option<fs::File> {
    static NEXT_CLONE: AtomicU64 = AtomicU64::new(0);
    const MAX_NAME_RETRIES: usize = 16;

    for _ in 0..MAX_NAME_RETRIES {
        let sequence = NEXT_CLONE.fetch_add(1, Ordering::Relaxed);
        let name = OsString::from(format!(
            ".haider-snapshot-{}-{sequence}.tmp",
            std::process::id()
        ));
        match rustix::fs::fclonefileat(source, parent, &name, rustix::fs::CloneFlags::empty()) {
            Ok(()) => {
                let clone = match rustix::fs::openat(
                    parent,
                    &name,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                ) {
                    Ok(clone) => fs::File::from(clone),
                    Err(_) => {
                        remove_temporary(parent, &name);
                        return None;
                    }
                };
                if rustix::fs::unlinkat(parent, &name, AtFlags::empty()).is_err() {
                    drop(clone);
                    remove_temporary(parent, &name);
                    return None;
                }
                return Some(clone);
            }
            Err(rustix::io::Errno::EXIST) => continue,
            Err(_) => return None,
        }
    }
    None
}

#[cfg(not(target_vendor = "apple"))]
fn try_clone_file_at(_parent: &OwnedFd, _source: &fs::File) -> Option<fs::File> {
    None
}

#[cfg(unix)]
fn snapshot_metadata_matches(before: &rustix::fs::Stat, after: &rustix::fs::Stat) -> bool {
    before.st_dev == after.st_dev
        && before.st_ino == after.st_ino
        && before.st_size == after.st_size
        && before.st_mtime == after.st_mtime
        && before.st_mtime_nsec == after.st_mtime_nsec
        && before.st_ctime == after.st_ctime
        && before.st_ctime_nsec == after.st_ctime_nsec
}

#[cfg(unix)]
fn require_unchanged_target(
    parent: &OwnedFd,
    leaf: &OsStr,
    expected: Option<&rustix::fs::Stat>,
    display_path: &Path,
) -> ToolResult<()> {
    match (
        expected,
        rustix::fs::statat(parent, leaf, AtFlags::SYMLINK_NOFOLLOW),
    ) {
        (Some(expected), Ok(current))
            if expected.st_dev == current.st_dev && expected.st_ino == current.st_ino =>
        {
            Ok(())
        }
        (None, Err(rustix::io::Errno::NOENT)) => Ok(()),
        (Some(_), Ok(_)) | (None, Ok(_)) | (Some(_), Err(rustix::io::Errno::NOENT)) => {
            Err(ToolError::PathChanged {
                path: display_path.to_path_buf(),
                message: "target identity changed before atomic replace".into(),
            })
        }
        (_, Err(error)) => Err(anchored_io_error(
            "recheck target before replace",
            display_path,
            error,
        )),
    }
}

#[cfg(unix)]
fn revalidate_commit_parent(
    workspace_dir: &OwnedFd,
    relative: &Path,
    held_parent: &OwnedFd,
    display_path: &Path,
) -> ToolResult<OwnedFd> {
    let workspace_root = workspace_root_from_target(display_path, relative).ok_or_else(|| {
        ToolError::PathChanged {
            path: display_path.to_path_buf(),
            message: "authorized target no longer identifies its workspace root".into(),
        }
    })?;
    let canonical_root =
        fs::canonicalize(&workspace_root).map_err(|error| ToolError::PathChanged {
            path: display_path.to_path_buf(),
            message: format!("workspace root changed before atomic replace: {error}"),
        })?;
    if canonical_root != workspace_root {
        return Err(ToolError::PathChanged {
            path: display_path.to_path_buf(),
            message: format!(
                "workspace root resolved to {} before atomic replace",
                canonical_root.display()
            ),
        });
    }
    let current_root = rustix::fs::openat(
        rustix::fs::CWD,
        &canonical_root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| anchored_io_error("reopen workspace root", display_path, error))?;
    require_same_directory(
        workspace_dir,
        &current_root,
        display_path,
        "workspace root identity changed before atomic replace",
    )?;

    let mut components = normal_components(relative);
    components.pop();
    let current_parent = walk_directories(
        current_root,
        &components,
        "reopen patch parent before replace",
        display_path,
    )?;
    require_same_directory(
        held_parent,
        &current_parent,
        display_path,
        "patch parent left its authorized workspace location before atomic replace",
    )?;
    require_commit_parent_path(&current_parent, display_path)?;
    Ok(current_parent)
}

#[cfg(unix)]
fn workspace_root_from_target(display_path: &Path, relative: &Path) -> Option<PathBuf> {
    let mut workspace_root = display_path.to_path_buf();
    for _ in normal_components(relative) {
        if !workspace_root.pop() {
            return None;
        }
    }
    Some(workspace_root)
}

#[cfg(unix)]
fn require_same_directory(
    expected: &OwnedFd,
    current: &OwnedFd,
    display_path: &Path,
    message: &'static str,
) -> ToolResult<()> {
    let expected = rustix::fs::fstat(expected)
        .map_err(|error| ToolError::io("inspect authorized directory", display_path, error))?;
    let current = rustix::fs::fstat(current)
        .map_err(|error| ToolError::io("inspect current directory", display_path, error))?;
    if expected.st_dev == current.st_dev && expected.st_ino == current.st_ino {
        return Ok(());
    }
    Err(ToolError::PathChanged {
        path: display_path.to_path_buf(),
        message: message.into(),
    })
}

#[cfg(target_vendor = "apple")]
fn require_commit_parent_path(parent: &OwnedFd, display_path: &Path) -> ToolResult<()> {
    let parent_path = rustix::fs::getpath(parent).map_err(|error| ToolError::PathChanged {
        path: display_path.to_path_buf(),
        message: format!("patch parent path changed before atomic replace: {error}"),
    })?;
    let parent_path = PathBuf::from(OsString::from_vec(parent_path.into_bytes()));
    if display_path
        .parent()
        .is_some_and(|expected_parent| parent_path == expected_parent)
    {
        return Ok(());
    }
    Err(ToolError::PathChanged {
        path: display_path.to_path_buf(),
        message: format!(
            "patch parent moved from its authorized workspace location before atomic replace: {}",
            parent_path.display()
        ),
    })
}

#[cfg(not(target_vendor = "apple"))]
fn require_commit_parent_path(_parent: &OwnedFd, _display_path: &Path) -> ToolResult<()> {
    Ok(())
}

/// Opens and exclusively locks the inode currently named by `leaf`.
///
/// A writer may have opened the old inode before another writer atomically
/// replaced the path. Comparing the locked fd's identity with an anchored
/// `statat` detects that stale-open race and retries before reading any bytes.
#[cfg(unix)]
fn open_locked_current_at(
    parent: &OwnedFd,
    leaf: &OsStr,
    display_path: &Path,
) -> ToolResult<(fs::File, rustix::fs::Stat)> {
    const MAX_STALE_OPEN_RETRIES: usize = 16;
    for _ in 0..MAX_STALE_OPEN_RETRIES {
        let source_fd =
            openat_nofollow(parent, leaf, OFlags::RDONLY, "open for patch", display_path)?;
        let source = fs::File::from(source_fd);
        source
            .lock()
            .map_err(|error| ToolError::io("lock for patch", display_path, error))?;
        let locked = rustix::fs::fstat(&source)
            .map_err(|error| ToolError::io("inspect locked patch", display_path, error))?;
        let current = rustix::fs::statat(parent, leaf, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|error| anchored_io_error("inspect current patch", display_path, error))?;
        if locked.st_dev == current.st_dev && locked.st_ino == current.st_ino {
            return Ok((source, locked));
        }
    }
    Err(ToolError::Runtime {
        message: format!(
            "patch target {} changed during {MAX_STALE_OPEN_RETRIES} lock attempts",
            display_path.display()
        ),
    })
}

#[cfg(unix)]
fn create_patch_temporary(
    parent: &OwnedFd,
    display_path: &Path,
) -> ToolResult<(OsString, OwnedFd)> {
    static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);
    const MAX_NAME_RETRIES: usize = 16;
    for _ in 0..MAX_NAME_RETRIES {
        let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
        let name = OsString::from(format!(
            ".haider-patch-{}-{sequence}.tmp",
            std::process::id()
        ));
        match rustix::fs::openat(
            parent,
            &name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        ) {
            Ok(file) => return Ok((name, file)),
            Err(rustix::io::Errno::EXIST) => {}
            Err(error) => {
                return Err(ToolError::io("create patch temporary", display_path, error));
            }
        }
    }
    Err(ToolError::Runtime {
        message: format!(
            "could not allocate a unique patch temporary for {}",
            display_path.display()
        ),
    })
}

#[cfg(unix)]
fn write_patch_temporary(
    temporary: OwnedFd,
    source_mode: rustix::fs::RawMode,
    patched: &[u8],
    display_path: &Path,
) -> ToolResult<()> {
    rustix::fs::fchmod(&temporary, Mode::from_raw_mode(source_mode))
        .map_err(|error| ToolError::io("set patch permissions", display_path, error))?;
    let mut temporary = fs::File::from(temporary);
    temporary
        .write_all(patched)
        .map_err(|error| ToolError::io("write patch temporary", display_path, error))?;
    temporary
        .sync_all()
        .map_err(|error| ToolError::io("sync patch temporary", display_path, error))
}

#[cfg(unix)]
fn remove_temporary(parent: &OwnedFd, name: &OsStr) {
    let _ = rustix::fs::unlinkat(parent, name, AtFlags::empty());
}

fn anchored_relative_path(workspace_root: &Path, canonical_path: &Path) -> ToolResult<PathBuf> {
    let relative =
        canonical_path
            .strip_prefix(workspace_root)
            .map_err(|_| ToolError::WorkspaceBoundary {
                workspace_root: workspace_root.to_path_buf(),
                requested_path: canonical_path.to_path_buf(),
                resolved_path: Some(canonical_path.to_path_buf()),
            })?;
    let mut checked = PathBuf::new();
    for component in relative.components() {
        match component {
            std::path::Component::Normal(component) => checked.push(component),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => {
                return Err(ToolError::WorkspaceBoundary {
                    workspace_root: workspace_root.to_path_buf(),
                    requested_path: relative.to_path_buf(),
                    resolved_path: Some(canonical_path.to_path_buf()),
                });
            }
        }
    }
    Ok(checked)
}

#[cfg(unix)]
fn open_target_at(
    workspace_dir: OwnedFd,
    relative: &Path,
    flags: OFlags,
    operation: &'static str,
    display_path: &Path,
) -> ToolResult<OwnedFd> {
    let mut components = normal_components(relative);
    let Some(leaf) = components.pop() else {
        return Ok(workspace_dir);
    };
    let parent = walk_directories(workspace_dir, &components, operation, display_path)?;
    openat_nofollow(&parent, &leaf, flags, operation, display_path)
}

#[cfg(unix)]
fn open_directory_at(
    workspace_dir: OwnedFd,
    relative: &Path,
    operation: &'static str,
    display_path: &Path,
) -> ToolResult<OwnedFd> {
    let components = normal_components(relative);
    walk_directories(workspace_dir, &components, operation, display_path)
}

#[cfg(unix)]
fn open_parent_at(
    workspace_dir: OwnedFd,
    relative: &Path,
    display_path: &Path,
) -> ToolResult<(OwnedFd, OsString)> {
    let mut components = normal_components(relative);
    let leaf = components
        .pop()
        .ok_or_else(|| ToolError::invalid_argument("filesystem path has no leaf name"))?;
    let parent = walk_directories(
        workspace_dir,
        &components,
        "open mutation parent",
        display_path,
    )?;
    Ok((parent, leaf))
}

#[cfg(unix)]
fn open_parent_creating_at(
    mut directory: OwnedFd,
    relative: &Path,
    display_path: &Path,
) -> ToolResult<(OwnedFd, OsString)> {
    let mut components = normal_components(relative);
    let leaf = components
        .pop()
        .ok_or_else(|| ToolError::invalid_argument("filesystem path has no leaf name"))?;
    for component in components {
        match rustix::fs::mkdirat(&directory, &component, Mode::from_raw_mode(0o755)) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
            Err(error) => {
                return Err(anchored_io_error(
                    "create write parent",
                    display_path,
                    error,
                ));
            }
        }
        directory = openat_nofollow(
            &directory,
            &component,
            OFlags::RDONLY | OFlags::DIRECTORY,
            "open write parent",
            display_path,
        )?;
    }
    Ok((directory, leaf))
}

fn normal_components(relative: &Path) -> Vec<OsString> {
    relative
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(component) => Some(component.to_os_string()),
            _ => None,
        })
        .collect()
}

#[cfg(unix)]
fn walk_directories(
    mut directory: OwnedFd,
    components: &[OsString],
    operation: &'static str,
    display_path: &Path,
) -> ToolResult<OwnedFd> {
    for component in components {
        directory = openat_nofollow(
            &directory,
            component,
            OFlags::RDONLY | OFlags::DIRECTORY,
            operation,
            display_path,
        )?;
    }
    Ok(directory)
}

#[cfg(unix)]
fn openat_nofollow(
    directory: &OwnedFd,
    path: impl rustix::path::Arg,
    flags: OFlags,
    operation: &'static str,
    display_path: &Path,
) -> ToolResult<OwnedFd> {
    rustix::fs::openat(
        directory,
        path,
        flags | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| anchored_io_error(operation, display_path, error))
}

#[cfg(unix)]
fn anchored_io_error(
    operation: &'static str,
    display_path: &Path,
    error: rustix::io::Errno,
) -> ToolError {
    if matches!(
        error,
        rustix::io::Errno::LOOP | rustix::io::Errno::NOENT | rustix::io::Errno::NOTDIR
    ) {
        ToolError::PathChanged {
            path: display_path.to_path_buf(),
            message: error.to_string(),
        }
    } else {
        ToolError::io(operation, display_path, error)
    }
}

#[cfg(unix)]
fn is_dot_entry(name: &CStr) -> bool {
    matches!(name.to_bytes(), b"." | b"..")
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
            status: haider_protocol::tool::ToolResultStatus::Completed,
            reason: None,
            presentation: None,
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
        status: haider_protocol::tool::ToolResultStatus::Completed,
        reason: None,
        presentation: None,
    })
}

async fn bounded_search<C>(
    output: SearchOutput,
    bounds: ResultBounds,
    cas: &mut C,
) -> ToolResult<BoundedResult>
where
    C: CasSink,
{
    let match_truncated = output.match_count > SEARCH_PREVIEW_MATCHES;
    let byte_truncated = output.total_bytes > bounds.max_preview_bytes;
    if !match_truncated && !byte_truncated {
        return Ok(BoundedResult {
            preview: output.preview,
            truncated: false,
            artifact: None,
            cursor: None,
            status: haider_protocol::tool::ToolResultStatus::Completed,
            reason: None,
            presentation: None,
        });
    }
    let artifact = cas.put_file(output.complete.path()).await?;
    Ok(BoundedResult {
        preview: output.preview,
        truncated: true,
        artifact: Some(artifact),
        cursor: None,
        status: haider_protocol::tool::ToolResultStatus::Completed,
        reason: None,
        presentation: None,
    })
}

async fn bounded_with_truncation<C>(
    contents: String,
    semantic_truncation: bool,
    bounds: ResultBounds,
    cas: &mut C,
) -> ToolResult<BoundedResult>
where
    C: CasSink,
{
    if !semantic_truncation {
        return bounded(contents, bounds, cas).await;
    }
    let artifact = if contents.len() > bounds.max_preview_bytes {
        Some(cas.put(contents.as_bytes()).await?)
    } else {
        None
    };
    Ok(BoundedResult {
        preview: utf8_prefix(&contents, bounds.max_preview_bytes).to_owned(),
        truncated: true,
        artifact,
        cursor: None,
        status: haider_protocol::tool::ToolResultStatus::Completed,
        reason: None,
        presentation: None,
    })
}

fn utf8_prefix(text: &str, max_bytes: usize) -> &str {
    let mut end = max_bytes.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
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

fn relative_path_argument(path: &Path) -> ToolResult<&str> {
    path_argument(path)
}

/// Single digest seam for authored content and structural mutation evidence.
/// A later workspace-revision producer can consume this value without
/// duplicating digest logic across mutation tools.
fn mutation_digest(bytes: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(bytes).to_hex())
}

fn case_mode_argument(mode: FsCaseMode) -> &'static str {
    match mode {
        FsCaseMode::Sensitive => "sensitive",
        FsCaseMode::Insensitive => "insensitive",
        FsCaseMode::Smart => "smart",
    }
}

fn search_mode_argument(mode: FsSearchMode) -> &'static str {
    match mode {
        FsSearchMode::Literal => "literal",
        FsSearchMode::Simple => "simple",
    }
}

#[derive(Debug, Clone, Copy)]
enum PathResolution {
    Existing,
    MissingPathOk,
    AnchoredLeaf,
    AnchoredExistingLeaf,
}

fn resolve_workspace_path(
    workspace_root: &Path,
    requested_path: &Path,
    resolution: PathResolution,
) -> ToolResult<PathBuf> {
    let resolved = match resolution {
        PathResolution::Existing => {
            let candidate = if requested_path.is_absolute() {
                requested_path.to_path_buf()
            } else {
                workspace_root.join(requested_path)
            };
            fs::canonicalize(&candidate)
                .map_err(|error| ToolError::io("canonicalize", &candidate, error))?
        }
        PathResolution::MissingPathOk => resolve_missing_path(workspace_root, requested_path)?,
        PathResolution::AnchoredLeaf | PathResolution::AnchoredExistingLeaf => {
            resolve_anchored_leaf(
                workspace_root,
                requested_path,
                matches!(resolution, PathResolution::AnchoredExistingLeaf),
            )?
        }
    };
    require_under_root(workspace_root, requested_path, &resolved)?;
    Ok(resolved)
}

fn resolve_missing_path(workspace_root: &Path, requested_path: &Path) -> ToolResult<PathBuf> {
    let candidate = if requested_path.is_absolute() {
        requested_path.to_path_buf()
    } else {
        workspace_root.join(requested_path)
    };
    if let Ok(resolved) = fs::canonicalize(&candidate) {
        require_under_root(workspace_root, requested_path, &resolved)?;
        return Ok(resolved);
    }

    let mut ancestor = candidate.clone();
    let mut missing_tail = Vec::new();
    let resolved_ancestor = loop {
        match fs::canonicalize(&ancestor) {
            Ok(resolved) => break resolved,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let leaf = ancestor
                    .file_name()
                    .ok_or_else(|| ToolError::WorkspaceBoundary {
                        workspace_root: workspace_root.to_path_buf(),
                        requested_path: requested_path.to_path_buf(),
                        resolved_path: None,
                    })?;
                if matches!(leaf.as_encoded_bytes(), b"." | b"..") {
                    return Err(ToolError::WorkspaceBoundary {
                        workspace_root: workspace_root.to_path_buf(),
                        requested_path: requested_path.to_path_buf(),
                        resolved_path: None,
                    });
                }
                missing_tail.push(leaf.to_os_string());
                ancestor.pop();
            }
            Err(error) => return Err(ToolError::io("canonicalize", &ancestor, error)),
        }
    };
    require_under_root(workspace_root, requested_path, &resolved_ancestor)?;
    let mut resolved = resolved_ancestor;
    for component in missing_tail.into_iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn resolve_anchored_leaf(
    workspace_root: &Path,
    requested_path: &Path,
    must_exist: bool,
) -> ToolResult<PathBuf> {
    let candidate = if requested_path.is_absolute() {
        requested_path.to_path_buf()
    } else {
        workspace_root.join(requested_path)
    };
    if fs::canonicalize(&candidate).is_ok_and(|path| path == workspace_root) {
        return Ok(workspace_root.to_path_buf());
    }
    let leaf = candidate
        .file_name()
        .ok_or_else(|| ToolError::invalid_argument("filesystem path has no leaf name"))?;
    let parent = candidate
        .parent()
        .ok_or_else(|| ToolError::invalid_argument("filesystem path has no parent"))?;
    let parent = fs::canonicalize(parent)
        .map_err(|error| ToolError::io("canonicalize parent", parent, error))?;
    require_under_root(workspace_root, requested_path, &parent)?;
    let resolved = parent.join(leaf);
    if must_exist {
        fs::symlink_metadata(&resolved)
            .map_err(|error| ToolError::io("inspect source", &resolved, error))?;
    }
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

#[cfg(all(test, unix))]
#[allow(clippy::expect_used)]
#[path = "filesystem/tests/w4a12.rs"]
mod w4a12_tests;

#[cfg(all(test, unix))]
#[allow(clippy::expect_used, unsafe_code)]
#[path = "filesystem/tests/w4a13.rs"]
mod w4a13_tests;

#[cfg(all(test, unix))]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, symlink};

    /// MUTATION CHECK (layered): Apple's atomic `RENAME_NOFOLLOW_ANY` is the
    /// load-bearing final seam; reverting that flag to plain `renameat`
    /// re-escapes and was verified in W4a1.1. This userspace parent recheck is
    /// defense-in-depth on Apple and rejects swaps already visible at recheck
    /// time on platforms/paths without that atomic flag; removing only it on
    /// Apple does not re-escape because the syscall independently rejects the
    /// destination symlink.
    #[test]
    fn rename_time_parent_escape_is_typed_path_change() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let workspace_path = directory.path().join("workspace");
        fs::create_dir(&workspace_path).expect("create workspace");
        let workspace_path = fs::canonicalize(workspace_path).expect("canonical workspace");
        let component = workspace_path.join("component");
        let escaped_component = directory.path().join("escaped-component");
        let target = component.join("target.txt");
        fs::create_dir(&component).expect("create workspace component");
        fs::write(&target, "before").expect("seed target");
        let workspace = rustix::fs::open(
            &workspace_path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .expect("open workspace");
        let operation = FsEdit::new(&target, "before", "after");
        let expected = mutation_digest(b"before");

        let result = apply_edit_at_before_replace(
            workspace,
            Path::new("component/target.txt"),
            &operation,
            Some(&expected),
            || {
                fs::rename(&component, &escaped_component)
                    .expect("move held parent outside workspace");
                symlink(&escaped_component, &component)
                    .expect("install outside-pointing component symlink");
            },
        );

        assert_eq!(
            fs::read_to_string(escaped_component.join("target.txt")).expect("read escaped target"),
            "before"
        );
        assert!(matches!(result, Err(ToolError::PathChanged { .. })));
        assert!(
            fs::read_dir(&escaped_component)
                .expect("read escaped component")
                .all(|entry| !entry
                    .expect("directory entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".haider-patch-"))
        );
    }

    /// MUTATION CHECK: replace `RENAME_NOFOLLOW_ANY` with plain `renameat`.
    /// Expected failure: the patch writes the moved outside target. Verified
    /// by revert in W4a1.1; this is the atomic layer named above.
    #[cfg(target_vendor = "apple")]
    #[test]
    fn parent_move_after_final_revalidation_is_refused_by_atomic_rename_resolution() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let workspace_path = directory.path().join("workspace");
        fs::create_dir(&workspace_path).expect("create workspace");
        let workspace_path = fs::canonicalize(workspace_path).expect("canonical workspace");
        let component = workspace_path.join("component");
        let escaped_component = directory.path().join("escaped-component");
        let target = component.join("target.txt");
        fs::create_dir(&component).expect("create workspace component");
        fs::write(&target, "before").expect("seed target");
        let workspace = rustix::fs::open(
            &workspace_path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .expect("open workspace");
        let operation = FsEdit::new(&target, "before", "after");
        let expected = mutation_digest(b"before");

        let result = apply_edit_at_with_commit_hooks(
            workspace,
            Path::new("component/target.txt"),
            &operation,
            Some(&expected),
            || {},
            || {
                fs::rename(&component, &escaped_component)
                    .expect("move validated parent outside workspace");
                symlink(&escaped_component, &component)
                    .expect("install outside-pointing component symlink");
            },
        );

        assert_eq!(
            fs::read_to_string(escaped_component.join("target.txt")).expect("read escaped target"),
            "before"
        );
        assert!(matches!(result, Err(ToolError::PathChanged { .. })));
    }

    /// MUTATION CHECK: remove the source-content hash recheck. Expected
    /// failure: the same-inode external edit is silently replaced by `haider`.
    /// Verified by revert in W4a1.1.
    #[test]
    fn same_inode_concurrent_edit_is_typed_path_change() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let workspace_path = fs::canonicalize(directory.path()).expect("canonical workspace");
        let target = workspace_path.join("target.txt");
        fs::write(&target, "before").expect("seed target");
        let initial_inode = fs::metadata(&target).expect("initial metadata").ino();
        let workspace = rustix::fs::open(
            &workspace_path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .expect("open workspace");
        let operation = FsEdit::new(&target, "before", "haider");
        let expected = mutation_digest(b"before");

        let result = apply_edit_at_before_replace(
            workspace,
            Path::new("target.txt"),
            &operation,
            Some(&expected),
            || {
                fs::write(&target, "editor").expect("rewrite target in place");
                assert_eq!(
                    fs::metadata(&target).expect("rewritten metadata").ino(),
                    initial_inode,
                    "reproduction must preserve the target inode"
                );
            },
        );

        assert_eq!(
            fs::read_to_string(&target).expect("read external target"),
            "editor"
        );
        assert!(matches!(result, Err(ToolError::PathChanged { .. })));
        assert!(
            fs::read_dir(directory.path())
                .expect("read temporary directory")
                .all(|entry| !entry
                    .expect("directory entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".haider-patch-"))
        );
    }

    #[test]
    fn external_leaf_replacement_before_edit_rename_is_typed_path_change() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let target = directory.path().join("target.txt");
        let parked = directory.path().join("parked.txt");
        fs::write(&target, "before").expect("seed target");
        let workspace = rustix::fs::open(
            directory.path(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .expect("open workspace");
        let operation = FsEdit::new(&target, "before", "haider");
        let expected = mutation_digest(b"before");

        let result = apply_edit_at_before_replace(
            workspace,
            Path::new("target.txt"),
            &operation,
            Some(&expected),
            || {
                fs::rename(&target, &parked).expect("replace original target");
                fs::write(&target, "external").expect("install external replacement");
            },
        );

        assert!(matches!(result, Err(ToolError::PathChanged { .. })));
        assert_eq!(
            fs::read_to_string(&target).expect("read external target"),
            "external"
        );
        assert_eq!(
            fs::read_to_string(&parked).expect("read parked target"),
            "before"
        );
        assert!(
            fs::read_dir(directory.path())
                .expect("read temporary directory")
                .all(|entry| !entry
                    .expect("directory entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".haider-patch-"))
        );
    }

    #[test]
    fn path_parent_escape_before_commit_is_typed_and_mutates_neither_location() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let workspace_path = directory.path().join("workspace");
        fs::create_dir(&workspace_path).expect("create workspace");
        let workspace_path = fs::canonicalize(workspace_path).expect("canonical workspace");
        let component = workspace_path.join("component");
        let escaped_component = directory.path().join("escaped-component");
        let source = component.join("source.txt");
        let destination = workspace_path.join("destination.txt");
        fs::create_dir(&component).expect("create source parent");
        fs::write(&source, "outside must stay unchanged").expect("seed source");
        let workspace = rustix::fs::open(
            &workspace_path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .expect("open workspace");
        let operation =
            FsPath::new(FsPathOperation::Move, &source).with_destination(destination.clone());

        let result = apply_path_at_before_mutation(
            workspace,
            Path::new("component/source.txt"),
            Some(Path::new("destination.txt")),
            &operation,
            || {
                fs::rename(&component, &escaped_component)
                    .expect("move held source parent outside workspace");
                fs::create_dir(&component).expect("replace source parent inside workspace");
                fs::write(component.join("source.txt"), "inside replacement")
                    .expect("seed replacement source");
            },
        );

        assert!(matches!(result, Err(ToolError::PathChanged { .. })));
        assert_eq!(
            fs::read_to_string(escaped_component.join("source.txt")).expect("read escaped source"),
            "outside must stay unchanged"
        );
        assert_eq!(
            fs::read_to_string(component.join("source.txt")).expect("read replacement source"),
            "inside replacement"
        );
        assert!(!destination.exists());
    }
}
