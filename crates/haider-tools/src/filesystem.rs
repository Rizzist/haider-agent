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
//!   journaled terminal outcome. `fs_read`, `fs_write`, and `fs_edit` attach
//!   the exact BLAKE3 digest to that outcome. Existing-file writes and edits
//!   compare it against the locked current snapshot, returning typed unread or
//!   stale refusals before any replacement is prepared.
//! - `fs_patch` proves its pre-image before writing (mismatch is a typed
//!   [`FsPatchConflict`]) and records applied writes in the
//!   [`crate::ChangeLedger`],
//!   attributed to the caller's `(session, turn)`, for the verify gate. The
//!   atomic rename, ledger append, and terminal outcome decision are one
//!   indivisible blocking critical section. A broker-owned finalizer always
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
//! - Patch pre-image and final-verify bytes use a same-directory `clonefile`
//!   COW snapshot when Apple provides one. If cloning is unavailable or fails,
//!   the read degrades to a metadata-guarded best effort; that portable fallback
//!   cannot exclude an undetectably torn read from a non-cooperating writer.
//!   The derived bytes go to a same-directory temp opened through the parent
//!   dirfd and land through `renameat`. Both `fs_write` and `fs_patch` hold the
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
use crate::{FsEditAnchorMismatch, FsPatchConflict, ToolError, ToolResult};
use async_trait::async_trait;
use haider_protocol::effect::{EffectClass, FileFreshness};
use haider_protocol::ids::{ArtifactRef, RunId, SessionId};
use haider_protocol::tool::BoundedResult;
use rustix::fd::OwnedFd;
use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use serde_json::{Value, json};
use std::collections::{BinaryHeap, HashMap};
use std::ffi::{CStr, OsStr, OsString};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStringExt;
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
    pub old_string: String,
    pub new_string: String,
    pub replace_all: bool,
}

impl FsEdit {
    pub fn new(
        path: impl Into<PathBuf>,
        old_string: impl Into<String>,
        new_string: impl Into<String>,
    ) -> Self {
        Self {
            path: path.into(),
            old_string: old_string.into(),
            new_string: new_string.into(),
            replace_all: false,
        }
    }

    pub fn replace_all(mut self, replace_all: bool) -> Self {
        self.replace_all = replace_all;
        self
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
            "new_string": self.new_string,
            "old_string": self.old_string,
            "path": path_argument(&self.path)?,
            "replace_all": self.replace_all,
        }))
    }

    fn canonical_arguments(&self, workspace_root: &Path) -> ToolResult<Value> {
        let path = resolve_workspace_path(workspace_root, &self.path, PathResolution::Existing)?;
        Ok(json!({
            "new_string": self.new_string,
            "old_string": self.old_string,
            "path": path_argument(&path)?,
            "replace_all": self.replace_all,
        }))
    }

    fn approval_preview(&self) -> Vec<String> {
        vec![
            format!("Target: {}", self.path.display()),
            format!(
                "Replace {} occurrence(s) of blake3:{} with {} UTF-8 bytes",
                if self.replace_all { "all" } else { "one" },
                blake3::hash(self.old_string.as_bytes()).to_hex(),
                self.new_string.len()
            ),
        ]
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
            resolve_workspace_path(workspace_root, &self.path, PathResolution::MissingLeafOk)?;
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

    fn approval_preview(&self) -> Vec<String> {
        let diff = format!(
            "--- expected\n+++ replacement\n{}\n{}",
            prefixed_lines('-', &self.preimage),
            prefixed_lines('+', &self.replacement)
        );
        vec![
            format!("Target: {}", self.path.display()),
            format!(
                "Structured exact-preimage hunk:\n{}",
                bounded_preview(&diff, 4 * 1024)
            ),
        ]
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
        )?);
        let relative = anchored_relative_path(self.workspace_root(), &operation.path)?;
        let display_path = operation.path.clone();
        let workspace_dir = self.duplicate_workspace_dir()?;
        let freshness_path = relative_path_argument(&relative)?.to_owned();
        let intent = self.begin(&operation, policy).await?;
        let read =
            run_blocking(move || read_utf8_at(workspace_dir, &relative, &display_path)).await;
        let (result, freshness) = match read {
            Ok(contents) => {
                let digest = digest_bytes(contents.as_bytes());
                let result = bounded(contents, bounds, cas).await;
                let freshness = result.as_ref().ok().map(|_| FileFreshness {
                    path: freshness_path,
                    digest,
                });
                (result, freshness)
            }
            Err(error) => (Err(error), None),
        };
        self.finish_with_freshness(&intent, result, freshness).await
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
        let relative = anchored_relative_path(self.workspace_root(), &operation.path)?;
        let display_path = operation.path.clone();
        let workspace_dir = self.duplicate_workspace_dir()?;
        self.bounded_read(&operation, policy, cas, bounds, move || {
            list_directory_at(workspace_dir, &relative, &display_path)
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
                PathResolution::MissingLeafOk,
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
            let (result, freshness) = match worker.await {
                Ok(outcome) => outcome.into_result_with_freshness(freshness_path),
                Err(error) if error.is_cancelled() => return None,
                Err(error) => (
                    Err(ToolError::Runtime {
                        message: format!("blocking filesystem worker failed: {error}"),
                    }),
                    None,
                ),
            };
            let result = finish.finish_with_freshness(result, freshness).await;
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
            old_string: operation.old_string.clone(),
            new_string: operation.new_string.clone(),
            replace_all: operation.replace_all,
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
            let (result, freshness) = match worker.await {
                Ok(outcome) => outcome.into_result_with_freshness(freshness_path),
                Err(error) if error.is_cancelled() => return None,
                Err(error) => (
                    Err(ToolError::Runtime {
                        message: format!("blocking filesystem worker failed: {error}"),
                    }),
                    None,
                ),
            };
            let result = finish.finish_with_freshness(result, freshness).await;
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

    pub async fn fs_patch<L>(
        &mut self,
        operation: &FsPatch,
        policy: &PermissionPolicy,
        attribution: &TurnAttribution,
        ledger: &L,
    ) -> ToolResult<BoundedResult>
    where
        L: ChangeLedgerSink,
    {
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
        let worker = tokio::task::spawn_blocking(move || {
            apply_patch_and_record(
                workspace_dir,
                &relative,
                &owned_operation,
                &critical_ledger,
                attribution,
                effect,
                summary,
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
            let result = match worker.await {
                Ok(outcome) => outcome.into_result(),
                Err(error) if error.is_cancelled() => return None,
                Err(error) => Err(ToolError::Runtime {
                    message: format!("blocking filesystem worker failed: {error}"),
                }),
            };
            let result = finish.finish(result).await;
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

fn read_utf8_at(
    workspace_dir: OwnedFd,
    relative: &Path,
    display_path: &Path,
) -> ToolResult<String> {
    let target = open_target_at(
        workspace_dir,
        relative,
        OFlags::RDONLY,
        "open for read",
        display_path,
    )?;
    read_utf8_file(fs::File::from(target), display_path)
}

fn read_utf8_file(mut file: fs::File, display_path: &Path) -> ToolResult<String> {
    let bytes =
        metadata_guarded_file_snapshot_with_reader(&mut file, display_path, |snapshot, buffer| {
            snapshot.read_at(buffer, 0)
        })?;
    String::from_utf8(bytes).map_err(|error| ToolError::InvalidArgument {
        message: format!("{} is not UTF-8 text: {error}", display_path.display()),
    })
}

fn list_directory_at(
    workspace_dir: OwnedFd,
    relative: &Path,
    display_path: &Path,
) -> ToolResult<String> {
    let directory = open_directory_at(workspace_dir, relative, "open for list", display_path)?;
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

/// Replaces the unique occurrence of the pre-image. Missing or ambiguous
/// pre-images fail as typed conflicts, so the model must widen an ambiguous
/// hunk with surrounding context and can never silently patch the wrong copy.
struct AppliedPatch {
    result: BoundedResult,
    path: PathBuf,
    bytes_hash: String,
}

enum PatchWorkerOutcome {
    Applied {
        result: BoundedResult,
        bytes_hash: String,
    },
    ApplyFailed(ToolError),
    LedgerFailed {
        error: ToolError,
        written: bool,
        bytes_hash: String,
    },
}

impl PatchWorkerOutcome {
    fn into_result(self) -> ToolResult<BoundedResult> {
        match self {
            Self::Applied { result, .. } => Ok(result),
            Self::ApplyFailed(error) => Err(error),
            Self::LedgerFailed { error, written, .. } => {
                debug_assert!(written, "ledger failure must follow a successful rename");
                Err(error)
            }
        }
    }

    fn into_result_with_freshness(
        self,
        relative_path: String,
    ) -> (ToolResult<BoundedResult>, Option<FileFreshness>) {
        match self {
            Self::Applied { result, bytes_hash } => (
                Ok(result),
                Some(FileFreshness {
                    path: relative_path,
                    digest: bytes_hash,
                }),
            ),
            Self::ApplyFailed(error) => (Err(error), None),
            Self::LedgerFailed {
                error,
                written,
                bytes_hash,
            } => {
                debug_assert!(written, "ledger failure must follow a successful rename");
                (
                    Err(error),
                    Some(FileFreshness {
                        path: relative_path,
                        digest: bytes_hash,
                    }),
                )
            }
        }
    }
}

struct MutationRecordContext<'a, L> {
    expected_digest: Option<&'a str>,
    ledger: &'a L,
    attribution: TurnAttribution,
    effect: haider_protocol::ids::EffectId,
    summary: String,
}

fn apply_write_and_record<L>(
    workspace_dir: OwnedFd,
    relative: &Path,
    operation: &FsWrite,
    context: MutationRecordContext<'_, L>,
) -> PatchWorkerOutcome
where
    L: ChangeLedgerSink,
{
    let applied = match apply_write_at(workspace_dir, relative, operation, context.expected_digest)
    {
        Ok(applied) => applied,
        Err(error) => return PatchWorkerOutcome::ApplyFailed(error),
    };
    let AppliedPatch {
        result,
        path,
        bytes_hash,
    } = applied;
    match context.ledger.record_fs_write(
        context.attribution.session,
        context.attribution.turn,
        FsWriteRecord {
            effect: context.effect,
            paths: vec![path],
            summary: context.summary,
            bytes_hash: bytes_hash.clone(),
        },
    ) {
        Ok(()) => PatchWorkerOutcome::Applied { result, bytes_hash },
        Err(error) => PatchWorkerOutcome::LedgerFailed {
            error,
            written: true,
            bytes_hash,
        },
    }
}

fn apply_write_at(
    workspace_dir: OwnedFd,
    relative: &Path,
    operation: &FsWrite,
    expected_digest: Option<&str>,
) -> ToolResult<AppliedPatch> {
    let traversal_root = rustix::io::dup(&workspace_dir)
        .map_err(|error| ToolError::io("duplicate workspace root", &operation.path, error))?;
    let (parent, leaf) = open_parent_at(traversal_root, relative, &operation.path)?;
    // Keep the advisory-exclusive lock on the current inode alive through
    // rename when overwriting. `fs_patch` enters through the same lock helper,
    // so every cooperating Haider mutation of an existing target serializes.
    // A missing leaf is valid create semantics; any other lookup error stays
    // typed and no-follow.
    let mut source = match rustix::fs::statat(&parent, &leaf, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => {
            let (mut source, metadata) = open_locked_current_at(&parent, &leaf, &operation.path)?;
            let (source_bytes, _source_basis) =
                file_snapshot(&parent, &mut source, &operation.path)?.parts();
            let current_digest = digest_bytes(&source_bytes);
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
    let bytes_hash = format!("blake3:{}", blake3::hash(bytes).to_hex());
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
    Ok(AppliedPatch {
        result: BoundedResult {
            preview: format!(
                "wrote {} bytes to {}",
                bytes.len(),
                operation.path.display()
            ),
            truncated: false,
            artifact: None,
            cursor: None,
        },
        path: operation.path.clone(),
        bytes_hash,
    })
}

fn apply_edit_and_record<L>(
    workspace_dir: OwnedFd,
    relative: &Path,
    operation: &FsEdit,
    context: MutationRecordContext<'_, L>,
) -> PatchWorkerOutcome
where
    L: ChangeLedgerSink,
{
    let applied = match apply_edit_at(workspace_dir, relative, operation, context.expected_digest) {
        Ok(applied) => applied,
        Err(error) => return PatchWorkerOutcome::ApplyFailed(error),
    };
    let AppliedPatch {
        result,
        path,
        bytes_hash,
    } = applied;
    match context.ledger.record_fs_write(
        context.attribution.session,
        context.attribution.turn,
        FsWriteRecord {
            effect: context.effect,
            paths: vec![path],
            summary: context.summary,
            bytes_hash: bytes_hash.clone(),
        },
    ) {
        Ok(()) => PatchWorkerOutcome::Applied { result, bytes_hash },
        Err(error) => PatchWorkerOutcome::LedgerFailed {
            error,
            written: true,
            bytes_hash,
        },
    }
}

fn apply_edit_at(
    workspace_dir: OwnedFd,
    relative: &Path,
    operation: &FsEdit,
    expected_digest: Option<&str>,
) -> ToolResult<AppliedPatch> {
    if operation.old_string.is_empty() {
        return Err(ToolError::invalid_argument(
            "fs_edit old_string cannot be empty",
        ));
    }
    let traversal_root = rustix::io::dup(&workspace_dir)
        .map_err(|error| ToolError::io("duplicate workspace root", &operation.path, error))?;
    let (parent, leaf) = open_parent_at(traversal_root, relative, &operation.path)?;
    let (mut source, source_metadata) = open_locked_current_at(&parent, &leaf, &operation.path)?;
    let (source_bytes, _source_basis) =
        file_snapshot(&parent, &mut source, &operation.path)?.parts();
    let current_digest = digest_bytes(&source_bytes);
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
    let contents = String::from_utf8(source_bytes).map_err(|error| ToolError::InvalidArgument {
        message: format!("{} is not UTF-8 text: {error}", operation.path.display()),
    })?;
    let matches = contents.match_indices(&operation.old_string).count();
    if (!operation.replace_all && matches != 1) || (operation.replace_all && matches == 0) {
        return Err(ToolError::EditAnchor(FsEditAnchorMismatch {
            path: operation.path.clone(),
            matches,
            replace_all: operation.replace_all,
        }));
    }
    let edited = if operation.replace_all {
        contents.replace(&operation.old_string, &operation.new_string)
    } else {
        contents.replacen(&operation.old_string, &operation.new_string, 1)
    };
    let bytes = edited.as_bytes();
    let bytes_hash = digest_bytes(bytes);
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
    Ok(AppliedPatch {
        result: BoundedResult {
            preview: format!(
                "edited {} ({} replacement{})",
                operation.path.display(),
                matches,
                if matches == 1 { "" } else { "s" }
            ),
            truncated: false,
            artifact: None,
            cursor: None,
        },
        path: operation.path.clone(),
        bytes_hash,
    })
}

fn apply_patch_and_record<L>(
    workspace_dir: OwnedFd,
    relative: &Path,
    operation: &FsPatch,
    ledger: &L,
    attribution: TurnAttribution,
    effect: haider_protocol::ids::EffectId,
    summary: String,
) -> PatchWorkerOutcome
where
    L: ChangeLedgerSink,
{
    let applied = match apply_patch_at(workspace_dir, relative, operation) {
        Ok(applied) => applied,
        Err(error) => return PatchWorkerOutcome::ApplyFailed(error),
    };
    let AppliedPatch {
        result,
        path,
        bytes_hash,
    } = applied;
    match ledger.record_fs_write(
        attribution.session,
        attribution.turn,
        FsWriteRecord {
            effect,
            paths: vec![path],
            summary,
            bytes_hash: bytes_hash.clone(),
        },
    ) {
        Ok(()) => PatchWorkerOutcome::Applied { result, bytes_hash },
        Err(error) => PatchWorkerOutcome::LedgerFailed {
            error,
            written: true,
            bytes_hash,
        },
    }
}

fn apply_patch_at(
    workspace_dir: OwnedFd,
    relative: &Path,
    operation: &FsPatch,
) -> ToolResult<AppliedPatch> {
    apply_patch_at_before_replace(workspace_dir, relative, operation, || {})
}

fn apply_patch_at_before_replace(
    workspace_dir: OwnedFd,
    relative: &Path,
    operation: &FsPatch,
    before_replace: impl FnOnce(),
) -> ToolResult<AppliedPatch> {
    apply_patch_at_with_commit_hooks(workspace_dir, relative, operation, before_replace, || {})
}

fn apply_patch_at_with_commit_hooks(
    workspace_dir: OwnedFd,
    relative: &Path,
    operation: &FsPatch,
    before_replace: impl FnOnce(),
    before_commit: impl FnOnce(),
) -> ToolResult<AppliedPatch> {
    if operation.preimage.is_empty() {
        return Err(ToolError::invalid_argument(
            "fs_patch preimage cannot be empty",
        ));
    }
    let traversal_root = rustix::io::dup(&workspace_dir)
        .map_err(|error| ToolError::io("duplicate workspace root", &operation.path, error))?;
    let (parent, leaf) = open_parent_at(traversal_root, relative, &operation.path)?;
    let (mut source, source_metadata) = open_locked_current_at(&parent, &leaf, &operation.path)?;
    let (source_bytes, _source_basis) =
        file_snapshot(&parent, &mut source, &operation.path)?.parts();
    let source_hash = blake3::hash(&source_bytes);
    let contents = String::from_utf8(source_bytes).map_err(|error| ToolError::InvalidArgument {
        message: format!("{} is not UTF-8 text: {error}", operation.path.display()),
    })?;
    let matches = contents
        .as_bytes()
        .windows(operation.preimage.len())
        .filter(|candidate| *candidate == operation.preimage.as_bytes())
        .count();
    if matches != 1 {
        return Err(ToolError::Conflict(FsPatchConflict {
            path: operation.path.clone(),
            expected_preimage: operation.preimage.clone(),
            matches,
        }));
    }
    let patched = contents
        .replacen(&operation.preimage, &operation.replacement, 1)
        .into_bytes();
    let bytes_hash = format!("blake3:{}", blake3::hash(&patched).to_hex());
    let (temporary_name, temporary_fd) = create_patch_temporary(&parent, &operation.path)?;
    let write_result = write_patch_temporary(
        temporary_fd,
        source_metadata.st_mode,
        &patched,
        &operation.path,
    );
    if let Err(error) = write_result {
        remove_temporary(&parent, &temporary_name);
        return Err(error);
    }
    let applied = AppliedPatch {
        result: BoundedResult {
            preview: format!("patched {}", operation.path.display()),
            truncated: false,
            artifact: None,
            cursor: None,
        },
        path: operation.path.clone(),
        bytes_hash,
    };
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
    // This is the final userspace content observation before the atomic
    // namespace operation. A successful same-directory clonefile gives the
    // verify a coherent COW basis immune to later writes to the original. If
    // cloning is unavailable or fails, the metadata-bracketed single read is
    // best-effort: MAP_SHARED writes, or ordinary writes on coarse-timestamp
    // filesystems, can tear it without detection. The advisory target lock
    // excludes cooperating writers in either case.
    if let Err(error) =
        require_unchanged_content(&parent, &mut source, source_hash, &operation.path)
    {
        remove_temporary(&parent, &temporary_name);
        return Err(error);
    }
    before_commit();
    // Repeat the anchored inode check after the (potentially retried) content
    // verification. This catches the common editor strategy of atomically
    // renaming a new inode over the target during that read.
    if let Err(error) = require_unchanged_target(
        &commit_parent,
        &leaf,
        Some(&source_metadata),
        &operation.path,
    ) {
        remove_temporary(&parent, &temporary_name);
        return Err(error);
    }
    // A non-cooperating writer can ignore the advisory lock. On every
    // filesystem, a same-inode write that lands after the CLONE INSTANT (the
    // clone read observes the older coherent snapshot, so the writer's newer
    // content is not seen by the verify), or a replacement inode installed
    // after this identity check, can still race the rename. The same-inode
    // check->rename window therefore begins at the clone instant, not after
    // the verify read completes. On a clonefile fallback, an undetectably torn
    // verify read is an additional residual. The exact bounds are ledgered in
    // docs/OPTIMIZATIONS.md under "haider-tools filesystem residual (W4a1.3)".
    if let Err(error) = replace_temporary_at_commit(
        &commit_parent,
        &temporary_name,
        &leaf,
        &operation.path,
        "replace patched file",
    ) {
        remove_temporary(&parent, &temporary_name);
        return Err(error);
    }
    drop(source);
    Ok(applied)
}

#[cfg(target_vendor = "apple")]
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

#[cfg(not(target_vendor = "apple"))]
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

fn snapshot_metadata_matches(before: &rustix::fs::Stat, after: &rustix::fs::Stat) -> bool {
    before.st_dev == after.st_dev
        && before.st_ino == after.st_ino
        && before.st_size == after.st_size
        && before.st_mtime == after.st_mtime
        && before.st_mtime_nsec == after.st_mtime_nsec
        && before.st_ctime == after.st_ctime
        && before.st_ctime_nsec == after.st_ctime_nsec
}

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

fn workspace_root_from_target(display_path: &Path, relative: &Path) -> Option<PathBuf> {
    let mut workspace_root = display_path.to_path_buf();
    for _ in normal_components(relative) {
        if !workspace_root.pop() {
            return None;
        }
    }
    Some(workspace_root)
}

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

fn open_directory_at(
    workspace_dir: OwnedFd,
    relative: &Path,
    operation: &'static str,
    display_path: &Path,
) -> ToolResult<OwnedFd> {
    let components = normal_components(relative);
    walk_directories(workspace_dir, &components, operation, display_path)
}

fn open_parent_at(
    workspace_dir: OwnedFd,
    relative: &Path,
    display_path: &Path,
) -> ToolResult<(OwnedFd, OsString)> {
    let mut components = normal_components(relative);
    let leaf = components
        .pop()
        .ok_or_else(|| ToolError::invalid_argument("fs_patch path has no file name"))?;
    let parent = walk_directories(
        workspace_dir,
        &components,
        "open patch parent",
        display_path,
    )?;
    Ok((parent, leaf))
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
        });
    }
    let artifact = cas.put_file(output.complete.path()).await?;
    Ok(BoundedResult {
        preview: output.preview,
        truncated: true,
        artifact: Some(artifact),
        cursor: None,
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

fn prefixed_lines(prefix: char, text: &str) -> String {
    text.split_inclusive('\n')
        .map(|line| format!("{prefix}{line}"))
        .collect()
}

fn bounded_preview(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [preview truncated]", &text[..end])
}

fn path_argument(path: &Path) -> ToolResult<&str> {
    path.to_str().ok_or_else(|| ToolError::InvalidArgument {
        message: format!("path is not valid UTF-8: {}", path.display()),
    })
}

fn relative_path_argument(path: &Path) -> ToolResult<&str> {
    path_argument(path)
}

fn digest_bytes(bytes: &[u8]) -> String {
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

#[cfg(test)]
#[allow(clippy::expect_used)]
#[path = "filesystem/tests/w4a12.rs"]
mod w4a12_tests;

#[cfg(test)]
#[allow(clippy::expect_used, unsafe_code)]
#[path = "filesystem/tests/w4a13.rs"]
mod w4a13_tests;

#[cfg(test)]
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
        let operation = FsPatch::new(&target, "before", "after");

        let result = apply_patch_at_before_replace(
            workspace,
            Path::new("component/target.txt"),
            &operation,
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
        let operation = FsPatch::new(&target, "before", "after");

        let result = apply_patch_at_with_commit_hooks(
            workspace,
            Path::new("component/target.txt"),
            &operation,
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
        let operation = FsPatch::new(&target, "before", "haider");

        let result =
            apply_patch_at_before_replace(workspace, Path::new("target.txt"), &operation, || {
                fs::write(&target, "editor").expect("rewrite target in place");
                assert_eq!(
                    fs::metadata(&target).expect("rewritten metadata").ino(),
                    initial_inode,
                    "reproduction must preserve the target inode"
                );
            });

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
    fn external_leaf_replacement_before_patch_rename_is_typed_path_change() {
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
        let operation = FsPatch::new(&target, "before", "haider");

        let result =
            apply_patch_at_before_replace(workspace, Path::new("target.txt"), &operation, || {
                fs::rename(&target, &parked).expect("replace original target");
                fs::write(&target, "external").expect("install external replacement");
            });

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
}
