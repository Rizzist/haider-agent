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
//! - Windows write/edit preserve the same freshness, blocking-critical-section,
//!   atomic-publication, ledger, and finalizer laws through their native path
//!   seam: component walks retain non-reparse directory handles without
//!   delete-sharing, target file locks serialize cooperating Haider writers,
//!   same-directory synced temporaries remain pinned and publish through
//!   handle-based Windows rename, and both parent/file identities and source
//!   bytes are revalidated immediately before publication. `fs_path` uses the
//!   same retained-parent boundary and stages recursive copies before their
//!   visible commit.
//!   As on non-Apple Unix, a non-cooperating namespace swap in the final
//!   userspace-check-to-rename gap remains an explicitly bounded limitation.

use crate::broker::{EffectBroker, EffectOperation, PermissionPolicy};
use crate::checkpoint::{
    CheckpointCapture, CheckpointCapturePath, FreezeCheckpointInput, checkpoint_without_cas,
    freeze_checkpoint,
};
use crate::ledger::{ChangeLedgerSink, FsWriteRecord};
use crate::{FsEditAnchorMismatch, ToolError, ToolResult};
use async_trait::async_trait;
use globset::{GlobBuilder, GlobMatcher};
use haider_platform::WorkspaceDirectory as OwnedFd;
use haider_protocol::checkpoint::{CheckpointKind, CheckpointOrigin};
use haider_protocol::effect::{EffectClass, FileFreshness, WorkspaceMutation};
use haider_protocol::ids::{ArtifactRef, BranchId, RunId, SessionId};
use haider_protocol::tool::{BoundedResult, DispatchMode, ToolManifest};
use haider_protocol::tool::{FsSearchMatch, ToolResultData, ToolTruncationReason};
use regex::{Regex, RegexBuilder};
use regex_syntax::ParserBuilder as RegexParserBuilder;
use regex_syntax::hir::{Capture, Hir, HirKind, Look};
#[cfg(unix)]
use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use serde_json::{Value, json};
use std::borrow::Cow;
#[cfg(test)]
use std::collections::HashSet;
use std::collections::{BinaryHeap, HashMap, VecDeque};
#[cfg(unix)]
use std::ffi::CStr;
#[cfg(unix)]
use std::ffi::OsStr;
#[cfg(unix)]
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader, Cursor, Read, Write};
#[cfg(windows)]
use std::io::{Seek, SeekFrom};
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
#[cfg(unix)]
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Port for storing the complete result when its prompt preview is truncated.
#[async_trait]
pub trait CasSink: Send {
    async fn put(&mut self, bytes: &[u8]) -> ToolResult<ArtifactRef>;
    /// Streams a staged file into CAS without rebuilding it as one buffer.
    async fn put_file(&mut self, path: &Path) -> ToolResult<ArtifactRef>;

    /// Validates, bounds, and stores one tool-produced PNG/JPEG. Lightweight
    /// test sinks may keep the honest unsupported default.
    async fn put_image(
        &mut self,
        _bytes: &[u8],
        _media_type: &str,
    ) -> ToolResult<haider_protocol::tool::ImageBlockRef> {
        Err(ToolError::cas(
            "this artifact sink does not support bounded image ingestion",
        ))
    }
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

/// Process-local storage is partitioned by the broker identity embedded in an
/// effect id, making entries session/worker scoped even though the cache lives
/// outside [`EffectBroker`] to keep the broker's durable state schema unchanged.
/// Only the declared-pure filesystem readers participate. Process execution
/// remains deliberately uncached; a future extension would need an explicit
/// declared-pure process mode rather than inferring purity from a command.
const READ_MEMO_CAP_BYTES: usize = 2 * 1024 * 1024;
const READ_MEMO_MAX_ENTRIES: usize = 256;
const READ_MEMO_ENTRY_OVERHEAD_BYTES: usize = 512;
const READ_FOOTPRINT_CAP_BYTES: usize = READ_MEMO_CAP_BYTES / 2;
const READ_FOOTPRINT_MAX_ENTRIES: usize = 8 * 1024;
const READ_FOOTPRINT_RESERVE_CHUNK: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ReadMemoCallKey {
    scope: String,
    workspace: PathBuf,
    tool: &'static str,
    args_digest: String,
    max_preview_bytes: usize,
}

#[derive(Clone)]
struct MemoizedRead {
    result: BoundedResult,
    freshness: Option<FileFreshness>,
    footprint: ReadFootprint,
}

#[derive(Clone)]
struct ReadMemoCandidate {
    generation: u64,
    footprint: ReadFootprint,
}

struct MemoizedReadHit {
    result: BoundedResult,
    freshness: Option<FileFreshness>,
}

struct ReadMemoEntry {
    generation: u64,
    value: MemoizedRead,
    weight: usize,
}

struct ReadMemo {
    cap_bytes: usize,
    used_bytes: usize,
    next_generation: u64,
    entries: HashMap<ReadMemoCallKey, ReadMemoEntry>,
    lru: VecDeque<ReadMemoCallKey>,
}

impl ReadMemo {
    fn new(cap_bytes: usize) -> Self {
        Self {
            cap_bytes,
            used_bytes: 0,
            next_generation: 0,
            entries: HashMap::new(),
            lru: VecDeque::new(),
        }
    }

    fn candidate(&self, key: &ReadMemoCallKey) -> Option<ReadMemoCandidate> {
        self.entries.get(key).map(|entry| ReadMemoCandidate {
            generation: entry.generation,
            footprint: entry.value.footprint.clone(),
        })
    }

    fn confirm(&mut self, key: &ReadMemoCallKey, generation: u64) -> Option<MemoizedReadHit> {
        let entry = self
            .entries
            .get(key)
            .filter(|entry| entry.generation == generation)?;
        let value = MemoizedReadHit {
            result: entry.value.result.clone(),
            freshness: entry.value.freshness.clone(),
        };
        self.touch(key);
        Some(value)
    }

    fn reject(&mut self, key: &ReadMemoCallKey, generation: u64) {
        if self
            .entries
            .get(key)
            .is_some_and(|entry| entry.generation == generation)
        {
            self.remove(key);
        }
    }

    fn insert(&mut self, key: ReadMemoCallKey, value: MemoizedRead) {
        self.remove(&key);
        let weight = read_memo_weight(&key, &value);
        if weight > self.cap_bytes {
            return;
        }
        while self.used_bytes.saturating_add(weight) > self.cap_bytes
            || self.entries.len() >= READ_MEMO_MAX_ENTRIES
        {
            let Some(oldest) = self.lru.pop_front() else {
                break;
            };
            if let Some(entry) = self.entries.remove(&oldest) {
                self.used_bytes = self.used_bytes.saturating_sub(entry.weight);
            }
        }
        self.next_generation = self.next_generation.wrapping_add(1);
        let generation = self.next_generation;
        self.used_bytes = self.used_bytes.saturating_add(weight);
        self.lru.push_back(key.clone());
        self.entries.insert(
            key,
            ReadMemoEntry {
                generation,
                value,
                weight,
            },
        );
    }

    fn invalidate_workspace(&mut self, workspace: &Path) {
        let keys = self
            .entries
            .keys()
            .filter(|key| key.workspace == workspace)
            .cloned()
            .collect::<Vec<_>>();
        for key in keys {
            self.remove(&key);
        }
    }

    fn touch(&mut self, key: &ReadMemoCallKey) {
        if let Some(position) = self.lru.iter().position(|candidate| candidate == key) {
            self.lru.remove(position);
        }
        self.lru.push_back(key.clone());
    }

    fn remove(&mut self, key: &ReadMemoCallKey) {
        if let Some(entry) = self.entries.remove(key) {
            self.used_bytes = self.used_bytes.saturating_sub(entry.weight);
        }
        if let Some(position) = self.lru.iter().position(|candidate| candidate == key) {
            self.lru.remove(position);
        }
    }
}

fn read_memo() -> &'static Mutex<ReadMemo> {
    static MEMO: OnceLock<Mutex<ReadMemo>> = OnceLock::new();
    MEMO.get_or_init(|| Mutex::new(ReadMemo::new(READ_MEMO_CAP_BYTES)))
}

fn with_read_memo<T>(operation: impl FnOnce(&mut ReadMemo) -> T) -> T {
    let mut memo = read_memo()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    operation(&mut memo)
}

fn read_memo_key(
    intent: &haider_protocol::effect::EffectIntent,
    workspace: &Path,
    tool: &'static str,
    bounds: ResultBounds,
) -> ReadMemoCallKey {
    let scope = intent
        .effect
        .as_str()
        .rsplit_once('-')
        .map_or_else(|| intent.effect.as_str(), |(scope, _)| scope)
        .to_owned();
    ReadMemoCallKey {
        scope,
        workspace: workspace.to_path_buf(),
        tool,
        args_digest: intent.args_digest.clone(),
        max_preview_bytes: bounds.max_preview_bytes,
    }
}

fn read_memo_weight(key: &ReadMemoCallKey, value: &MemoizedRead) -> usize {
    let result = &value.result;
    let result_bytes = result
        .preview
        .capacity()
        .saturating_add(result.cursor.as_ref().map_or(0, String::capacity))
        .saturating_add(result.reason.as_ref().map_or(0, String::capacity))
        .saturating_add(
            result
                .artifact
                .as_ref()
                .map_or(0, |artifact| artifact.0.capacity()),
        )
        .saturating_add(
            result
                .data
                .as_ref()
                .and_then(|data| serde_json::to_vec(data).ok())
                .map_or(0, |encoded| encoded.len()),
        );
    let freshness_bytes = value.freshness.as_ref().map_or(0, |freshness| {
        freshness.path.capacity() + freshness.digest.capacity()
    });
    let footprint_bytes = value.footprint.retained_bytes();
    // The call key is owned once by the map and once by the exact LRU queue.
    let key_bytes = key
        .scope
        .capacity()
        .saturating_add(key.workspace.capacity())
        .saturating_add(key.args_digest.capacity())
        .saturating_mul(2);
    READ_MEMO_ENTRY_OVERHEAD_BYTES
        .saturating_add(key_bytes)
        .saturating_add(result_bytes)
        .saturating_add(freshness_bytes)
        .saturating_add(footprint_bytes)
}

fn invalidate_read_memo(workspace: &Path) {
    with_read_memo(|memo| memo.invalidate_workspace(workspace));
}

pub fn fs_read_manifest() -> ToolManifest {
    ToolManifest {
        name: "fs_read".into(),
        description: "Read a redacted, bounded UTF-8 file slice or list a directory; use offset/limit for range reads and the artifact handle for full owner-authorized bytes".into(),
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
        description: "List redacted workspace files matching a bounded repository-aware glob"
            .into(),
        effects: vec![EffectClass::FsRead],
        dispatch: DispatchMode::Await,
        input_schema: json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "minLength": 1, "maxLength": GLOB_PATTERN_MAX_BYTES},
                "path": {"type": "string", "minLength": 1},
                "respect_gitignore": {"type": "boolean", "default": true},
                "include_hidden": {"type": "boolean", "default": false}
            },
            "required": ["pattern"],
            "additionalProperties": false
        }),
    }
}

pub fn fs_search_manifest() -> ToolManifest {
    ToolManifest {
        name: "fs_search".into(),
        description: "Search redacted, bounded repository file contents with literal, simple, or safe Rust-regex matching".into(),
        effects: vec![EffectClass::FsRead],
        dispatch: DispatchMode::Await,
        input_schema: json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "minLength": 1, "maxLength": SEARCH_PATTERN_MAX_BYTES},
                "path": {"type": "string", "minLength": 1},
                "glob": {"type": "string", "minLength": 1, "maxLength": GLOB_PATTERN_MAX_BYTES},
                "case": {"type": "string", "enum": ["sensitive", "insensitive", "smart"]},
                "mode": {"type": "string", "enum": ["literal", "simple", "regex"], "description": "regex sources are additionally capped at 1024 UTF-8 bytes before Unicode expansion"},
                "multiline": {"type": "boolean", "default": false, "description": "Make ^ and $ match physical line boundaries in the line-streamed regex engine"},
                "context": {
                    "type": "object",
                    "properties": {
                        "before": {"type": "integer", "minimum": 0, "maximum": 5},
                        "after": {"type": "integer", "minimum": 0, "maximum": 5}
                    },
                    "additionalProperties": false
                },
                "max_matches": {"type": "integer", "minimum": 1, "maximum": SEARCH_PREVIEW_MATCHES},
                "file_glob": {
                    "type": "object",
                    "properties": {
                        "include": {"type": "array", "items": {"type": "string", "minLength": 1, "maxLength": GLOB_PATTERN_MAX_BYTES}, "maxItems": 32},
                        "exclude": {"type": "array", "items": {"type": "string", "minLength": 1, "maxLength": GLOB_PATTERN_MAX_BYTES}, "maxItems": 32}
                    },
                    "additionalProperties": false
                },
                "respect_gitignore": {"type": "boolean", "default": true},
                "include_hidden": {"type": "boolean", "default": false},
                "query": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": SEARCH_PATTERN_MAX_BYTES,
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
    pub branch: Option<BranchId>,
    pub call_id: String,
}

impl TurnAttribution {
    pub fn new(session: SessionId, turn: RunId) -> Self {
        Self {
            session,
            turn,
            branch: None,
            call_id: "unknown-call".into(),
        }
    }

    pub fn with_tool_call(mut self, branch: Option<BranchId>, call_id: impl Into<String>) -> Self {
        self.branch = branch;
        self.call_id = call_id.into();
        self
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
    pub multiline: bool,
    pub context: FsSearchContext,
    pub max_matches: usize,
    pub file_glob: FsFileGlob,
    pub respect_gitignore: bool,
    pub include_hidden: bool,
}

impl FsSearch {
    pub fn new(root: impl Into<PathBuf>, query: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            query: query.into(),
            glob: None,
            case_mode: FsCaseMode::Sensitive,
            mode: FsSearchMode::Literal,
            multiline: false,
            context: FsSearchContext::default(),
            max_matches: SEARCH_PREVIEW_MATCHES,
            file_glob: FsFileGlob::default(),
            respect_gitignore: true,
            include_hidden: false,
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

    pub fn with_multiline(mut self, multiline: bool) -> Self {
        self.multiline = multiline;
        self
    }

    pub fn with_context(mut self, before: usize, after: usize) -> Self {
        self.context = FsSearchContext { before, after };
        self
    }

    pub fn with_max_matches(mut self, max_matches: usize) -> Self {
        self.max_matches = max_matches;
        self
    }

    pub fn with_file_glob(mut self, file_glob: FsFileGlob) -> Self {
        self.file_glob = file_glob;
        self
    }

    pub fn with_repo_options(mut self, respect_gitignore: bool, include_hidden: bool) -> Self {
        self.respect_gitignore = respect_gitignore;
        self.include_hidden = include_hidden;
        self
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FsSearchContext {
    pub before: usize,
    pub after: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FsFileGlob {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

impl FsFileGlob {
    pub fn new(include: Vec<String>, exclude: Vec<String>) -> Self {
        Self { include, exclude }
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
    /// Compatibility wildcards: `*` matches any run and `?` one scalar. The
    /// pattern is translated once into the same bounded linear-time engine.
    Simple,
    /// Rust's linear-time regex engine, compiled with explicit syntax, NFA,
    /// nesting, and DFA cache limits before any file is opened.
    Regex,
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
            "multiline": self.multiline,
            "context": {"before": self.context.before, "after": self.context.after},
            "max_matches": self.max_matches,
            "file_glob": {"include": self.file_glob.include, "exclude": self.file_glob.exclude},
            "respect_gitignore": self.respect_gitignore,
            "include_hidden": self.include_hidden,
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
            "multiline": self.multiline,
            "context": {"before": self.context.before, "after": self.context.after},
            "max_matches": self.max_matches,
            "file_glob": {"include": self.file_glob.include, "exclude": self.file_glob.exclude},
            "respect_gitignore": self.respect_gitignore,
            "include_hidden": self.include_hidden,
            "query": self.query,
            "root": path_argument(&root)?,
        }))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsGlob {
    pub root: PathBuf,
    pub pattern: String,
    pub respect_gitignore: bool,
    pub include_hidden: bool,
}

impl FsGlob {
    pub fn new(root: impl Into<PathBuf>, pattern: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            pattern: pattern.into(),
            respect_gitignore: true,
            include_hidden: false,
        }
    }

    pub fn with_repo_options(mut self, respect_gitignore: bool, include_hidden: bool) -> Self {
        self.respect_gitignore = respect_gitignore;
        self.include_hidden = include_hidden;
        self
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
            "respect_gitignore": self.respect_gitignore,
            "include_hidden": self.include_hidden,
        }))
    }

    fn canonical_arguments(&self, workspace_root: &Path) -> ToolResult<Value> {
        let root = resolve_workspace_path(workspace_root, &self.root, PathResolution::Existing)?;
        Ok(json!({
            "pattern": self.pattern,
            "root": path_argument(&root)?,
            "respect_gitignore": self.respect_gitignore,
            "include_hidden": self.include_hidden,
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
        let memo_key = read_memo_key(&intent, self.workspace_root(), "fs_read", bounds);
        if let Ok(memo_dir) = self.duplicate_workspace_dir()
            && let Some(cached) = lookup_memoized_read(&memo_key, memo_dir).await
        {
            return self
                .finish_with_freshness(&intent, Ok(cached.result), cached.freshness)
                .await;
        }
        let offset = operation.offset;
        let limit = operation.limit;
        let read = run_blocking(move || {
            read_path_at(workspace_dir, &relative, &display_path, offset, limit)
        })
        .await;
        let (result, freshness, footprint) = match read {
            Ok(read) => {
                let footprint = read.footprint;
                let sensitive_path = crate::redact::is_sensitive_path(&operation.path)
                    || (crate::redact::is_token_config_path(&operation.path)
                        && crate::redact::token_config_contains_secret(read.contents.as_bytes()));
                let result = bounded_read(
                    read.contents,
                    read.preview_contents,
                    read.data,
                    sensitive_path,
                    bounds,
                    cas,
                )
                .await;
                let freshness = result.as_ref().ok().and_then(|_| {
                    read.digest.map(|digest| FileFreshness {
                        path: freshness_path,
                        digest,
                    })
                });
                (result, freshness, Some(footprint))
            }
            Err(error) => (Err(error), None, None),
        };
        let cached = result
            .as_ref()
            .ok()
            .zip(footprint)
            .map(|(result, footprint)| MemoizedRead {
                result: result.clone(),
                freshness: freshness.clone(),
                footprint,
            });
        let result = self.finish_with_freshness(&intent, result, freshness).await;
        if result.is_ok()
            && let Some(cached) = cached
            && let Ok(memo_dir) = self.duplicate_workspace_dir()
        {
            insert_memoized_read_if_current(memo_key, cached, memo_dir).await;
        }
        result
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
        .with_mode(operation.mode)
        .with_multiline(operation.multiline)
        .with_context(operation.context.before, operation.context.after)
        .with_max_matches(operation.max_matches)
        .with_file_glob(operation.file_glob.clone())
        .with_repo_options(operation.respect_gitignore, operation.include_hidden);
        if let Some(glob) = requested_glob {
            operation = operation.with_glob(glob);
        }
        let owned = operation.clone();
        let relative = anchored_relative_path(self.workspace_root(), &operation.root)?;
        let workspace_dir = self.duplicate_workspace_dir()?;
        let workspace_root = self.workspace_root().to_path_buf();
        let max_matches = owned.max_matches;
        let intent = self.begin(&operation, policy).await?;
        let memo_key = read_memo_key(&intent, self.workspace_root(), "fs_search", bounds);
        if let Ok(memo_dir) = self.duplicate_workspace_dir()
            && let Some(cached) = lookup_memoized_read(&memo_key, memo_dir).await
        {
            return self.finish(&intent, Ok(cached.result)).await;
        }
        let wall_started = Instant::now();
        let permit = tokio::time::timeout(
            SEARCH_WALL_TIME_BUDGET,
            search_worker_gate().acquire_owned(),
        )
        .await;
        let result = match permit {
            Ok(Ok(permit)) => {
                let mut worker = tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    search_files_at(
                        workspace_dir,
                        &workspace_root,
                        &relative,
                        &owned,
                        bounds.max_preview_bytes,
                    )
                });
                let remaining = SEARCH_WALL_TIME_BUDGET.saturating_sub(wall_started.elapsed());
                match tokio::time::timeout(remaining, &mut worker).await {
                    Ok(joined) => joined.unwrap_or_else(|error| {
                        Err(ToolError::Runtime {
                            message: format!("blocking search worker failed: {error}"),
                        })
                    }),
                    Err(_) => timed_out_search(bounds.max_preview_bytes, max_matches),
                }
            }
            Ok(Err(error)) => Err(ToolError::Runtime {
                message: format!("search worker gate closed: {error}"),
            }),
            Err(_) => timed_out_search(bounds.max_preview_bytes, max_matches),
        };
        let result = result.and_then(|matches| {
            if matches.truncated_reason == Some(ToolTruncationReason::TimeBudget) {
                timed_out_search_output(bounds.max_preview_bytes, max_matches)
            } else {
                Ok(matches)
            }
        });
        let (result, footprint) = match result {
            Ok(mut matches) => {
                let footprint = matches.footprint.take();
                (bounded_search(matches, bounds, cas).await, footprint)
            }
            Err(error) => (Err(error), None),
        };
        let cached = result
            .as_ref()
            .ok()
            .zip(footprint)
            .map(|(result, footprint)| MemoizedRead {
                result: result.clone(),
                freshness: None,
                footprint,
            });
        let result = self.finish(&intent, result).await;
        if result.is_ok()
            && let Some(cached) = cached
            && let Ok(memo_dir) = self.duplicate_workspace_dir()
        {
            insert_memoized_read_if_current(memo_key, cached, memo_dir).await;
        }
        result
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
        )
        .with_repo_options(operation.respect_gitignore, operation.include_hidden);
        let owned = operation.clone();
        let relative = anchored_relative_path(self.workspace_root(), &operation.root)?;
        let workspace_dir = self.duplicate_workspace_dir()?;
        let workspace_root = self.workspace_root().to_path_buf();
        let intent = self.begin(&operation, policy).await?;
        let memo_key = read_memo_key(&intent, self.workspace_root(), "fs_glob", bounds);
        if let Ok(memo_dir) = self.duplicate_workspace_dir()
            && let Some(cached) = lookup_memoized_read(&memo_key, memo_dir).await
        {
            return self.finish(&intent, Ok(cached.result)).await;
        }
        let result =
            run_blocking(move || glob_files_at(workspace_dir, &workspace_root, &relative, &owned))
                .await;
        let (result, footprint) = match result {
            Ok(mut output) => {
                let footprint = output.footprint.take();
                (bounded_glob(output, bounds, cas).await, footprint)
            }
            Err(error) => (Err(error), None),
        };
        let cached = result
            .as_ref()
            .ok()
            .zip(footprint)
            .map(|(result, footprint)| MemoizedRead {
                result: result.clone(),
                freshness: None,
                footprint,
            });
        let result = self.finish(&intent, result).await;
        if result.is_ok()
            && let Some(cached) = cached
            && let Ok(memo_dir) = self.duplicate_workspace_dir()
        {
            insert_memoized_read_if_current(memo_key, cached, memo_dir).await;
        }
        result
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
        self.fs_write_with_checkpoint_cas(operation, policy, attribution, ledger, None)
            .await
    }

    pub async fn fs_write_checkpointed<C, L>(
        &mut self,
        operation: &FsWrite,
        policy: &PermissionPolicy,
        attribution: &TurnAttribution,
        ledger: &L,
        cas: C,
    ) -> ToolResult<BoundedResult>
    where
        C: CasSink + 'static,
        L: ChangeLedgerSink,
    {
        self.fs_write_with_checkpoint_cas(
            operation,
            policy,
            attribution,
            ledger,
            Some(Box::new(cas)),
        )
        .await
    }

    async fn fs_write_with_checkpoint_cas<L>(
        &mut self,
        operation: &FsWrite,
        policy: &PermissionPolicy,
        attribution: &TurnAttribution,
        ledger: &L,
        mut checkpoint_cas: Option<Box<dyn CasSink>>,
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
        if let Err(error) = self
            .require_checkpoint_support(checkpoint_cas.is_some())
            .await
        {
            return self.finish(&intent, Err(error)).await;
        }
        // A dispatched workspace mutation may land even when its terminal
        // ledger/result path later fails. Evict before the mutation worker
        // starts so no reader can reuse a pre-mutation entry through that
        // uncertainty.
        invalidate_read_memo(self.workspace_root());
        let relative = anchored_relative_path(self.workspace_root(), &operation.path);
        let workspace_dir = self.duplicate_workspace_dir();
        let owned_operation = operation.clone();
        let critical_ledger = ledger.clone();
        let attribution = attribution.clone();
        let checkpoint_attribution = attribution.clone();
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
            let (mut result, freshness, workspace_mutation, capture) = match worker.await {
                Ok(outcome) => outcome.into_result_with_freshness(freshness_path),
                Err(error) if error.is_cancelled() => return None,
                Err(error) => (
                    Err(ToolError::Runtime {
                        message: format!("blocking filesystem worker failed: {error}"),
                    }),
                    None,
                    None,
                    None,
                ),
            };
            let checkpoint = match capture {
                Some(capture) => {
                    let input = checkpoint_freeze_input(&checkpoint_attribution, &intent.effect);
                    match checkpoint_cas.as_deref_mut() {
                        Some(cas) => match freeze_checkpoint(cas, input, capture.clone()).await {
                            Ok(checkpoint) => Some(checkpoint),
                            Err(error) => {
                                let reason = error.to_string();
                                let input = checkpoint_freeze_input(
                                    &checkpoint_attribution,
                                    &intent.effect,
                                );
                                result = Err(error);
                                Some(checkpoint_without_cas(input, capture, &reason))
                            }
                        },
                        None => match finish.freeze_checkpoint(input, capture.clone()).await {
                            Ok(checkpoint) => Some(checkpoint),
                            Err(error) => {
                                let reason = error.to_string();
                                let input = checkpoint_freeze_input(
                                    &checkpoint_attribution,
                                    &intent.effect,
                                );
                                result = Err(error);
                                Some(checkpoint_without_cas(input, capture, &reason))
                            }
                        },
                    }
                }
                None => None,
            };
            let result = finish
                .finish_with_checkpoint(result, freshness, workspace_mutation, checkpoint)
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
        self.fs_edit_with_checkpoint_cas(operation, policy, attribution, ledger, None)
            .await
    }

    pub async fn fs_edit_checkpointed<C, L>(
        &mut self,
        operation: &FsEdit,
        policy: &PermissionPolicy,
        attribution: &TurnAttribution,
        ledger: &L,
        cas: C,
    ) -> ToolResult<BoundedResult>
    where
        C: CasSink + 'static,
        L: ChangeLedgerSink,
    {
        self.fs_edit_with_checkpoint_cas(
            operation,
            policy,
            attribution,
            ledger,
            Some(Box::new(cas)),
        )
        .await
    }

    async fn fs_edit_with_checkpoint_cas<L>(
        &mut self,
        operation: &FsEdit,
        policy: &PermissionPolicy,
        attribution: &TurnAttribution,
        ledger: &L,
        mut checkpoint_cas: Option<Box<dyn CasSink>>,
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
        if let Err(error) = self
            .require_checkpoint_support(checkpoint_cas.is_some())
            .await
        {
            return self.finish(&intent, Err(error)).await;
        }
        invalidate_read_memo(self.workspace_root());
        let relative = anchored_relative_path(self.workspace_root(), &operation.path);
        let workspace_dir = self.duplicate_workspace_dir();
        let owned_operation = operation.clone();
        let critical_ledger = ledger.clone();
        let attribution = attribution.clone();
        let checkpoint_attribution = attribution.clone();
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
            let (mut result, freshness, workspace_mutation, capture) = match worker.await {
                Ok(outcome) => outcome.into_result_with_freshness(freshness_path),
                Err(error) if error.is_cancelled() => return None,
                Err(error) => (
                    Err(ToolError::Runtime {
                        message: format!("blocking filesystem worker failed: {error}"),
                    }),
                    None,
                    None,
                    None,
                ),
            };
            let checkpoint = match capture {
                Some(capture) => {
                    let input = checkpoint_freeze_input(&checkpoint_attribution, &intent.effect);
                    match checkpoint_cas.as_deref_mut() {
                        Some(cas) => match freeze_checkpoint(cas, input, capture.clone()).await {
                            Ok(checkpoint) => Some(checkpoint),
                            Err(error) => {
                                let reason = error.to_string();
                                let input = checkpoint_freeze_input(
                                    &checkpoint_attribution,
                                    &intent.effect,
                                );
                                result = Err(error);
                                Some(checkpoint_without_cas(input, capture, &reason))
                            }
                        },
                        None => match finish.freeze_checkpoint(input, capture.clone()).await {
                            Ok(checkpoint) => Some(checkpoint),
                            Err(error) => {
                                let reason = error.to_string();
                                let input = checkpoint_freeze_input(
                                    &checkpoint_attribution,
                                    &intent.effect,
                                );
                                result = Err(error);
                                Some(checkpoint_without_cas(input, capture, &reason))
                            }
                        },
                    }
                }
                None => None,
            };
            let result = finish
                .finish_with_checkpoint(result, freshness, workspace_mutation, checkpoint)
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
        self.fs_path_with_checkpoint_cas(operation, policy, attribution, ledger, None)
            .await
    }

    pub async fn fs_path_checkpointed<C, L>(
        &mut self,
        operation: &FsPath,
        policy: &PermissionPolicy,
        attribution: &TurnAttribution,
        ledger: &L,
        cas: C,
    ) -> ToolResult<BoundedResult>
    where
        C: CasSink + 'static,
        L: ChangeLedgerSink,
    {
        self.fs_path_with_checkpoint_cas(
            operation,
            policy,
            attribution,
            ledger,
            Some(Box::new(cas)),
        )
        .await
    }

    async fn fs_path_with_checkpoint_cas<L>(
        &mut self,
        operation: &FsPath,
        policy: &PermissionPolicy,
        attribution: &TurnAttribution,
        ledger: &L,
        mut checkpoint_cas: Option<Box<dyn CasSink>>,
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
        if let Err(error) = self
            .require_checkpoint_support(checkpoint_cas.is_some())
            .await
        {
            return self.finish(&intent, Err(error)).await;
        }
        invalidate_read_memo(self.workspace_root());
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
        let checkpoint_attribution = attribution.clone();
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
            let (mut result, workspace_mutation, capture) = match worker.await {
                Ok(outcome) => outcome.into_result(),
                Err(error) if error.is_cancelled() => return None,
                Err(error) => (
                    Err(ToolError::Runtime {
                        message: format!("blocking filesystem worker failed: {error}"),
                    }),
                    None,
                    None,
                ),
            };
            let checkpoint = match capture {
                Some(capture) => {
                    let input = checkpoint_freeze_input(&checkpoint_attribution, &intent.effect);
                    match checkpoint_cas.as_deref_mut() {
                        Some(cas) => match freeze_checkpoint(cas, input, capture.clone()).await {
                            Ok(checkpoint) => Some(checkpoint),
                            Err(error) => {
                                let reason = error.to_string();
                                let input = checkpoint_freeze_input(
                                    &checkpoint_attribution,
                                    &intent.effect,
                                );
                                result = Err(error);
                                Some(checkpoint_without_cas(input, capture, &reason))
                            }
                        },
                        None => match finish.freeze_checkpoint(input, capture.clone()).await {
                            Ok(checkpoint) => Some(checkpoint),
                            Err(error) => {
                                let reason = error.to_string();
                                let input = checkpoint_freeze_input(
                                    &checkpoint_attribution,
                                    &intent.effect,
                                );
                                result = Err(error);
                                Some(checkpoint_without_cas(input, capture, &reason))
                            }
                        },
                    }
                }
                None => None,
            };
            let result = finish
                .finish_with_checkpoint(result, None, workspace_mutation, checkpoint)
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReadFootprint {
    entries: Vec<FreshnessStamp>,
    digest: String,
}

impl ReadFootprint {
    fn new(entries: Vec<FreshnessStamp>) -> Self {
        let mut hasher = blake3::Hasher::new();
        for entry in &entries {
            entry.update_digest(&mut hasher);
        }
        Self {
            entries,
            digest: format!("blake3:{}", hasher.finalize().to_hex()),
        }
    }

    fn retained_bytes(&self) -> usize {
        let entry_storage = self
            .entries
            .capacity()
            .saturating_mul(std::mem::size_of::<FreshnessStamp>());
        self.entries.iter().fold(
            self.digest.capacity().saturating_add(entry_storage),
            |bytes, entry| bytes.saturating_add(entry.path.capacity()),
        )
    }
}

struct ReadFootprintBuilder {
    entries: Vec<FreshnessStamp>,
    path_bytes: usize,
    cacheable: bool,
}

impl ReadFootprintBuilder {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            path_bytes: 0,
            cacheable: true,
        }
    }

    fn push(&mut self, entry: FreshnessStamp) {
        if !self.cacheable {
            return;
        }
        let path_bytes = entry.path.capacity();
        if self.entries.len() >= READ_FOOTPRINT_MAX_ENTRIES
            || self
                .path_bytes
                .saturating_add(path_bytes)
                .saturating_add(
                    self.entries
                        .capacity()
                        .saturating_mul(std::mem::size_of::<FreshnessStamp>()),
                )
                .saturating_add(READ_MEMO_ENTRY_OVERHEAD_BYTES)
                > READ_FOOTPRINT_CAP_BYTES
        {
            self.disable();
            return;
        }
        if self.entries.len() == self.entries.capacity() {
            let additional = READ_FOOTPRINT_RESERVE_CHUNK
                .min(READ_FOOTPRINT_MAX_ENTRIES.saturating_sub(self.entries.len()));
            let projected_capacity = self.entries.capacity().saturating_add(additional);
            let projected = self
                .path_bytes
                .saturating_add(path_bytes)
                .saturating_add(
                    projected_capacity.saturating_mul(std::mem::size_of::<FreshnessStamp>()),
                )
                .saturating_add(READ_MEMO_ENTRY_OVERHEAD_BYTES);
            if projected > READ_FOOTPRINT_CAP_BYTES
                || self.entries.try_reserve_exact(additional).is_err()
            {
                self.disable();
                return;
            }
        }
        self.path_bytes = self.path_bytes.saturating_add(path_bytes);
        self.entries.push(entry);
    }

    fn finish(self) -> Option<ReadFootprint> {
        self.cacheable.then(|| ReadFootprint::new(self.entries))
    }

    fn is_cacheable(&self) -> bool {
        self.cacheable
    }

    fn disable(&mut self) {
        self.entries = Vec::new();
        self.path_bytes = 0;
        self.cacheable = false;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FreshnessStamp {
    path: PathBuf,
    metadata: FreshnessMetadata,
}

impl FreshnessStamp {
    fn update_digest(&self, hasher: &mut blake3::Hasher) {
        let path = self.path.as_os_str().as_encoded_bytes();
        hasher.update(&(path.len() as u64).to_le_bytes());
        hasher.update(path);
        self.metadata.update_digest(hasher);
    }
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FreshnessMetadata {
    device: u64,
    inode: u64,
    mode: u64,
    size: i64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

#[cfg(unix)]
impl FreshnessMetadata {
    fn from_stat(metadata: &rustix::fs::Stat) -> Self {
        Self {
            device: stat_field_u64(metadata.st_dev),
            inode: metadata.st_ino,
            mode: metadata.st_mode as u64,
            size: metadata.st_size,
            modified_seconds: metadata.st_mtime,
            modified_nanoseconds: stat_field_i64(metadata.st_mtime_nsec),
            changed_seconds: metadata.st_ctime,
            changed_nanoseconds: stat_field_i64(metadata.st_ctime_nsec),
        }
    }

    fn update_digest(self, hasher: &mut blake3::Hasher) {
        for field in [
            self.device,
            self.inode,
            self.mode,
            self.size as u64,
            self.modified_seconds as u64,
            self.modified_nanoseconds as u64,
            self.changed_seconds as u64,
            self.changed_nanoseconds as u64,
        ] {
            hasher.update(&field.to_le_bytes());
        }
    }
}

#[cfg(unix)]
fn stat_field_i64<N>(field: N) -> i64
where
    N: Into<i128>,
{
    field.into() as i64
}

#[cfg(unix)]
fn stat_field_u64<N>(field: N) -> u64
where
    N: Into<i128>,
{
    field.into() as u64
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FreshnessMetadata(WindowsPathIdentity);

#[cfg(windows)]
impl FreshnessMetadata {
    fn update_digest(self, hasher: &mut blake3::Hasher) {
        hasher.update(format!("{:?}", self.0).as_bytes());
    }
}

async fn lookup_memoized_read(
    key: &ReadMemoCallKey,
    workspace_dir: OwnedFd,
) -> Option<MemoizedReadHit> {
    let candidate = with_read_memo(|memo| memo.candidate(key))?;
    let footprint = candidate.footprint;
    let current = run_blocking(move || Ok(read_footprint_is_current(workspace_dir, &footprint)))
        .await
        .unwrap_or(false);
    with_read_memo(|memo| {
        if current {
            memo.confirm(key, candidate.generation)
        } else {
            memo.reject(key, candidate.generation);
            None
        }
    })
}

async fn insert_memoized_read_if_current(
    key: ReadMemoCallKey,
    value: MemoizedRead,
    workspace_dir: OwnedFd,
) {
    let footprint = value.footprint.clone();
    let current = run_blocking(move || Ok(read_footprint_is_current(workspace_dir, &footprint)))
        .await
        .unwrap_or(false);
    if current {
        with_read_memo(|memo| memo.insert(key, value));
    }
}

#[cfg(unix)]
fn read_footprint_is_current(workspace_dir: OwnedFd, footprint: &ReadFootprint) -> bool {
    let mut hasher = blake3::Hasher::new();
    for expected in &footprint.entries {
        let Ok(root) = rustix::io::dup(&workspace_dir) else {
            return false;
        };
        let Ok(metadata) = freshness_stat_at(root, &expected.path) else {
            return false;
        };
        let current = FreshnessStamp {
            path: expected.path.clone(),
            metadata: FreshnessMetadata::from_stat(&metadata),
        };
        if current != *expected {
            return false;
        }
        current.update_digest(&mut hasher);
    }
    format!("blake3:{}", hasher.finalize().to_hex()) == footprint.digest
}

#[cfg(unix)]
fn freshness_stat_at(workspace_dir: OwnedFd, relative: &Path) -> ToolResult<rustix::fs::Stat> {
    let mut components = normal_components(relative);
    let Some(leaf) = components.pop() else {
        return rustix::fs::fstat(&workspace_dir)
            .map_err(|error| ToolError::io("validate cached read", relative, error));
    };
    let parent = walk_directories(
        workspace_dir,
        &components,
        "validate cached read parent",
        relative,
    )?;
    rustix::fs::statat(&parent, &leaf, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|error| anchored_io_error("validate cached read", relative, error))
}

#[cfg(windows)]
fn read_footprint_is_current(workspace_dir: OwnedFd, footprint: &ReadFootprint) -> bool {
    let mut hasher = blake3::Hasher::new();
    for expected in &footprint.entries {
        let Ok((_parent, _target, entry)) =
            windows_anchored_entry(&workspace_dir, &expected.path, &expected.path)
        else {
            return false;
        };
        let current = FreshnessStamp {
            path: expected.path.clone(),
            metadata: FreshnessMetadata(entry.identity),
        };
        if current != *expected {
            return false;
        }
        current.update_digest(&mut hasher);
    }
    format!("blake3:{}", hasher.finalize().to_hex()) == footprint.digest
}

struct ReadPathOutput {
    contents: String,
    preview_contents: Option<String>,
    digest: Option<String>,
    footprint: ReadFootprint,
    data: Option<ToolResultData>,
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
    let footprint = || {
        ReadFootprint::new(vec![FreshnessStamp {
            path: relative.to_path_buf(),
            metadata: FreshnessMetadata::from_stat(&metadata),
        }])
    };
    match FileType::from_raw_mode(metadata.st_mode) {
        FileType::RegularFile => {
            let contents = read_utf8_file(fs::File::from(target), display_path)?;
            let digest = mutation_digest(contents.as_bytes());
            let preview_contents = if offset.is_some() || limit.is_some() {
                Some(select_numbered_lines(
                    &crate::redact::redact_private_key_lines(&contents).text,
                    offset.unwrap_or(1),
                    limit,
                ))
            } else {
                None
            };
            let contents = if offset.is_some() || limit.is_some() {
                select_numbered_lines(&contents, offset.unwrap_or(1), limit)
            } else {
                contents
            };
            Ok(ReadPathOutput {
                contents,
                preview_contents,
                digest: Some(digest),
                footprint: footprint(),
                data: None,
            })
        }
        FileType::Directory => {
            let listing = list_directory_fd(target, display_path)?;
            Ok(ReadPathOutput {
                contents: listing.contents,
                preview_contents: None,
                digest: None,
                footprint: footprint(),
                data: Some(listing.data),
            })
        }
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

/// Shallow directory listings share the glob entry cap before first send.
pub const FS_DIRECTORY_ENTRY_LIMIT: usize = GLOB_ENTRY_LIMIT;
const DIRECTORY_EXTENSION_COLLAPSE_THRESHOLD: usize = 8;
const DIRECTORY_EXTENSION_EXAMPLES: usize = 3;

struct DirectoryListing {
    contents: String,
    data: ToolResultData,
}

struct DirectoryCollector {
    entries: BinaryHeap<(String, bool)>,
    entries_seen: usize,
}

impl DirectoryCollector {
    fn new() -> Self {
        Self {
            entries: BinaryHeap::new(),
            entries_seen: 0,
        }
    }

    fn push(&mut self, name: String, is_directory: bool) {
        self.entries_seen = self.entries_seen.saturating_add(1);
        if self.entries.len() < FS_DIRECTORY_ENTRY_LIMIT {
            self.entries.push((name, is_directory));
        } else if self
            .entries
            .peek()
            .is_some_and(|largest| name.as_str() < largest.0.as_str())
        {
            self.entries.pop();
            self.entries.push((name, is_directory));
        }
    }

    fn finish(self) -> DirectoryListing {
        directory_listing(self.entries.into_sorted_vec(), self.entries_seen)
    }
}

#[cfg(unix)]
fn list_directory_fd(directory: OwnedFd, display_path: &Path) -> ToolResult<DirectoryListing> {
    let mut entries = rustix::fs::Dir::new(directory)
        .map_err(|error| ToolError::io("list", display_path, error))?;
    let mut listed = DirectoryCollector::new();
    while let Some(entry) = entries.read() {
        let entry = entry.map_err(|error| ToolError::io("list", display_path, error))?;
        if is_dot_entry(entry.file_name()) {
            continue;
        }
        let mut name = entry.file_name().to_string_lossy().into_owned();
        let is_directory = entry.file_type() == FileType::Directory;
        if is_directory {
            name.push('/');
        }
        listed.push(name, is_directory);
    }
    Ok(listed.finish())
}

fn directory_listing(entries: Vec<(String, bool)>, entries_seen: usize) -> DirectoryListing {
    let raw = entries
        .iter()
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();
    let collapsed_entries = directory_preview(&entries).1;
    DirectoryListing {
        contents: join_lines(raw),
        data: ToolResultData::FsRead {
            truncated_reason: if entries_seen > FS_DIRECTORY_ENTRY_LIMIT {
                Some(ToolTruncationReason::EntryLimit)
            } else if collapsed_entries > 0 {
                Some(ToolTruncationReason::PresentationReduced)
            } else {
                None
            },
            entries_seen,
            collapsed_entries,
        },
    }
}

fn directory_preview(entries: &[(String, bool)]) -> (String, usize) {
    let mut extension_counts = HashMap::<String, usize>::new();
    for (name, directory) in entries {
        if !directory
            && let Some(extension) = Path::new(name).extension().and_then(|value| value.to_str())
        {
            *extension_counts
                .entry(extension.to_ascii_lowercase())
                .or_insert(0) += 1;
        }
    }
    let mut extension_seen = HashMap::<String, usize>::new();
    let mut preview = Vec::new();
    let mut collapsed = 0usize;
    for (name, directory) in entries {
        let path = Path::new(name.trim_end_matches('/'));
        if crate::redact::is_sensitive_path(path) || crate::redact::is_token_config_path(path) {
            preview.push("[REDACTED:sensitive_path]".to_owned());
            collapsed = collapsed.saturating_add(1);
            continue;
        }
        if *directory
            && matches!(
                name.trim_end_matches('/').to_ascii_lowercase().as_str(),
                "node_modules" | "target" | "vendor" | ".venv" | "dist"
            )
        {
            preview.push(format!("{name} [collapsed vendor directory]"));
            collapsed = collapsed.saturating_add(1);
            continue;
        }
        let Some(extension) = path
            .extension()
            .and_then(|value| value.to_str())
            .map(str::to_ascii_lowercase)
        else {
            preview.push(name.clone());
            continue;
        };
        let count = extension_counts.get(&extension).copied().unwrap_or(0);
        let seen = extension_seen.entry(extension.clone()).or_insert(0);
        *seen = seen.saturating_add(1);
        if count < DIRECTORY_EXTENSION_COLLAPSE_THRESHOLD || *seen <= DIRECTORY_EXTENSION_EXAMPLES {
            preview.push(name.clone());
        } else if *seen == DIRECTORY_EXTENSION_EXAMPLES.saturating_add(1) {
            let omitted = count.saturating_sub(DIRECTORY_EXTENSION_EXAMPLES);
            preview.push(format!("[… {omitted} more .{extension} files]"));
            collapsed = collapsed.saturating_add(omitted);
        }
    }
    (join_lines(preview), collapsed)
}

fn search_files_at(
    workspace_dir: OwnedFd,
    workspace_root: &Path,
    relative: &Path,
    operation: &FsSearch,
    max_preview_bytes: usize,
) -> ToolResult<SearchOutput> {
    let started = Instant::now();
    validate_search(operation)?;
    let compiled = CompiledSearch::new(operation)?;
    let path_filters = CompiledPathFilters::new(operation)?;
    let walked = crate::repo::walk_files(
        workspace_root,
        &operation.root,
        crate::repo::WalkOptions {
            respect_gitignore: operation.respect_gitignore,
            include_hidden: operation.include_hidden,
            max_files: SEARCH_MAX_ENUMERATED_FILES,
            deadline: Some(started + SEARCH_WALL_TIME_BUDGET),
        },
    )?;
    let mut footprint =
        if !operation.respect_gitignore && !walked.truncated && !walked.time_budget_reached {
            repository_read_footprint(&workspace_dir, &walked.directories, &walked.files)
        } else {
            None
        };
    let mut matches = SearchCollector::new(max_preview_bytes, operation.max_matches)?;
    matches.skipped_sensitive = walked
        .hidden_sensitive_files
        .iter()
        .filter_map(|path| path_under_search_root(workspace_root, relative, path).ok())
        .filter_map(|path| portable_relative_path(path).ok())
        .filter(|path| path_filters.matches(path))
        .count();
    if walked.time_budget_reached {
        matches.truncate(ToolTruncationReason::TimeBudget);
    }
    if walked.truncated {
        matches.truncate(ToolTruncationReason::EnumerationLimit);
    }
    for workspace_path in walked.files {
        if started.elapsed() >= SEARCH_WALL_TIME_BUDGET {
            matches.truncate(ToolTruncationReason::TimeBudget);
            break;
        }
        let path_under_root = path_under_search_root(workspace_root, relative, &workspace_path)?;
        let match_path = portable_relative_path(path_under_root)?;
        if !path_filters.matches(&match_path) {
            continue;
        }
        if crate::redact::is_sensitive_path(&workspace_path) {
            matches.skip_sensitive();
            continue;
        }
        if matches.files_scanned == SEARCH_MAX_FILES {
            matches.truncate(ToolTruncationReason::FilesScanned);
            break;
        }
        let entry_path = workspace_root.join(&workspace_path);
        let file = open_search_file(&workspace_dir, &workspace_path, &entry_path)?;
        matches.file_scanned();
        collect_streamed_file_matches(
            file,
            &workspace_path,
            &entry_path,
            operation,
            &compiled,
            started,
            &mut matches,
        )?;
        if matches.truncated_reason == Some(ToolTruncationReason::TimeBudget)
            || matches.truncated_reason == Some(ToolTruncationReason::BytesScanned)
            || matches.truncated_reason == Some(ToolTruncationReason::ResultBytes)
            || matches.truncated_reason == Some(ToolTruncationReason::LineTooLong)
            || matches.truncated_reason == Some(ToolTruncationReason::MatchLimit)
        {
            break;
        }
    }
    if matches.truncated_reason == Some(ToolTruncationReason::TimeBudget) {
        footprint = None;
    }
    Ok(matches.finish(footprint))
}

/// Maximum structured/preview matches accepted from one search call.
pub const SEARCH_PREVIEW_MATCHES: usize = 200;
/// Maximum stable paths returned by one glob call.
pub const GLOB_ENTRY_LIMIT: usize = 500;
/// Hard file-enumeration cap for one search.
pub const SEARCH_MAX_FILES: usize = 10_000;
/// File-enumeration ceiling before content/path filters are applied.
pub const SEARCH_MAX_ENUMERATED_FILES: usize = 100_000;
/// Hard total content bytes inspected by one search.
pub const SEARCH_MAX_SCANNED_BYTES: usize = 16 * 1024 * 1024;
/// A single physical line never occupies more than this much resident memory.
pub const SEARCH_MAX_LINE_BYTES: usize = 256 * 1024;
/// First-file bytes inspected for binary NUL sniffing.
pub const SEARCH_BINARY_SNIFF_BYTES: usize = 8 * 1024;
/// Hard observable search deadline. Internal checks stop cooperatively; an
/// outer async timeout returns an empty typed TimeBudget result. A one-per-
/// process gate prevents non-interruptible regex/directory-sort workers from
/// accumulating while the bounded timed-out worker finishes off-thread.
pub const SEARCH_WALL_TIME_BUDGET: Duration = Duration::from_secs(2);

fn search_worker_gate() -> Arc<tokio::sync::Semaphore> {
    static GATE: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    Arc::clone(GATE.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(1))))
}

/// Aggregate raw-spool/structured budget and hard serialized first-send cap.
pub const SEARCH_MAX_RESULT_BYTES: usize = 4 * 1024 * 1024;
/// Conservative envelope/counter allowance in total-result accounting.
pub const SEARCH_RESULT_ACCOUNTING_OVERHEAD: usize = 1024;
/// Per-line ceiling inside structured match/context fields.
pub const SEARCH_STRUCTURED_LINE_BYTES: usize = 4 * 1024;
/// Regex syntax/NFA construction limit.
pub const SEARCH_REGEX_SIZE_LIMIT: usize = 1024 * 1024;
/// Lazy DFA cache limit; regex falls back safely rather than growing freely.
pub const SEARCH_REGEX_DFA_SIZE_LIMIT: usize = 2 * 1024 * 1024;
/// Parser nesting limit for adversarial grouped expressions.
pub const SEARCH_REGEX_NEST_LIMIT: u32 = 128;
/// Maximum UTF-8 bytes accepted in any search pattern.
pub const SEARCH_PATTERN_MAX_BYTES: usize = 64 * 1024;
/// Tighter untrusted regex-source ceiling before Unicode HIR expansion. This
/// bounds the pre-RegexBuilder boundary-rewrite stage to a small fixed heap.
pub const SEARCH_REGEX_PATTERN_MAX_BYTES: usize = 1024;
/// The wildcard compatibility mode has a smaller source ceiling.
pub const SEARCH_SIMPLE_PATTERN_MAX_BYTES: usize = 4 * 1024;
/// Maximum UTF-8 bytes in any path glob before compilation.
pub const GLOB_PATTERN_MAX_BYTES: usize = 1024;
/// Enumeration cap for glob before the 500-entry presentation reduction.
pub const GLOB_MAX_FILES_SCANNED: usize = 50_000;

struct SearchOutput {
    preview: String,
    preview_saturated: bool,
    match_count: usize,
    max_matches: usize,
    total_bytes: usize,
    complete: tempfile::NamedTempFile,
    footprint: Option<ReadFootprint>,
    structured: Vec<FsSearchMatch>,
    truncated_reason: Option<ToolTruncationReason>,
    binary_files_skipped: usize,
    skipped_sensitive: usize,
    files_scanned: usize,
    bytes_scanned: usize,
}

struct SearchCollector {
    preview: String,
    match_count: usize,
    total_bytes: usize,
    max_preview_bytes: usize,
    max_matches: usize,
    preview_saturated: bool,
    complete: tempfile::NamedTempFile,
    structured: Vec<FsSearchMatch>,
    structured_bytes: usize,
    truncated_reason: Option<ToolTruncationReason>,
    binary_files_skipped: usize,
    skipped_sensitive: usize,
    files_scanned: usize,
    bytes_scanned: usize,
}

fn timed_out_search(max_preview_bytes: usize, max_matches: usize) -> ToolResult<SearchOutput> {
    timed_out_search_output(max_preview_bytes, max_matches)
}

fn timed_out_search_output(
    max_preview_bytes: usize,
    max_matches: usize,
) -> ToolResult<SearchOutput> {
    let mut collector = SearchCollector::new(max_preview_bytes, max_matches)?;
    collector.truncate(ToolTruncationReason::TimeBudget);
    Ok(collector.finish(None))
}

impl SearchCollector {
    fn new(max_preview_bytes: usize, max_matches: usize) -> ToolResult<Self> {
        let complete = tempfile::NamedTempFile::new()
            .map_err(|error| ToolError::io("create search result spool", "<search>", error))?;
        Ok(Self {
            preview: String::new(),
            match_count: 0,
            total_bytes: 0,
            max_preview_bytes,
            max_matches,
            preview_saturated: false,
            complete,
            structured: Vec::new(),
            structured_bytes: 0,
            truncated_reason: None,
            binary_files_skipped: 0,
            skipped_sensitive: 0,
            files_scanned: 0,
            bytes_scanned: 0,
        })
    }

    fn push_line(
        &mut self,
        raw_line: &str,
        preview_line: &str,
        mut structured: Vec<FsSearchMatch>,
    ) -> ToolResult<Vec<usize>> {
        let projected_bytes = self
            .total_bytes
            .saturating_add(raw_line.len())
            .saturating_add(1);
        if projected_bytes
            .saturating_add(self.structured_bytes)
            .saturating_add(SEARCH_RESULT_ACCOUNTING_OVERHEAD)
            > SEARCH_MAX_RESULT_BYTES
        {
            self.truncate(ToolTruncationReason::ResultBytes);
            return Ok(Vec::new());
        }
        self.complete
            .write_all(raw_line.as_bytes())
            .and_then(|()| self.complete.write_all(b"\n"))
            .map_err(|error| ToolError::io("write search result spool", "<search>", error))?;
        self.total_bytes = projected_bytes;
        let first_match_index = self.match_count;
        self.match_count = self.match_count.saturating_add(structured.len());
        if self.match_count > self.max_matches {
            self.truncate(ToolTruncationReason::MatchLimit);
        }
        if first_match_index < self.max_matches
            && self.preview.len() < self.max_preview_bytes
            && !self.preview_saturated
        {
            let remaining = self.max_preview_bytes - self.preview.len();
            self.preview.push_str(utf8_prefix(preview_line, remaining));
            if preview_line.len() < remaining {
                self.preview.push('\n');
            } else {
                self.preview_saturated = true;
            }
        }
        let remaining = self.max_matches.saturating_sub(self.structured.len());
        structured.truncate(remaining);
        let start = self.structured.len();
        for found in structured {
            let retained_bytes = structured_match_bytes(&found)?;
            if self
                .total_bytes
                .saturating_add(self.structured_bytes)
                .saturating_add(retained_bytes)
                .saturating_add(SEARCH_RESULT_ACCOUNTING_OVERHEAD)
                > SEARCH_MAX_RESULT_BYTES
            {
                self.truncate(ToolTruncationReason::ResultBytes);
                break;
            }
            self.structured_bytes = self.structured_bytes.saturating_add(retained_bytes);
            self.structured.push(found);
        }
        Ok((start..self.structured.len()).collect())
    }

    fn append_context_after(&mut self, index: usize, line: &str) -> ToolResult<()> {
        let Some(found) = self.structured.get(index) else {
            return Ok(());
        };
        let old_bytes = structured_match_bytes(found)?;
        let mut candidate = found.clone();
        candidate.context_after.push(line.to_owned());
        let new_bytes = structured_match_bytes(&candidate)?;
        let projected = self
            .total_bytes
            .saturating_add(self.structured_bytes.saturating_sub(old_bytes))
            .saturating_add(new_bytes)
            .saturating_add(SEARCH_RESULT_ACCOUNTING_OVERHEAD);
        if projected > SEARCH_MAX_RESULT_BYTES {
            self.truncate(ToolTruncationReason::ResultBytes);
            return Ok(());
        }
        self.structured_bytes = self
            .structured_bytes
            .saturating_sub(old_bytes)
            .saturating_add(new_bytes);
        self.structured[index] = candidate;
        Ok(())
    }

    fn add_bytes_scanned(&mut self, bytes: usize) {
        self.bytes_scanned = self.bytes_scanned.saturating_add(bytes);
    }

    fn file_scanned(&mut self) {
        self.files_scanned = self.files_scanned.saturating_add(1);
    }

    fn skip_binary(&mut self) {
        self.binary_files_skipped = self.binary_files_skipped.saturating_add(1);
    }

    fn skip_sensitive(&mut self) {
        self.skipped_sensitive = self.skipped_sensitive.saturating_add(1);
    }

    fn truncate(&mut self, reason: ToolTruncationReason) {
        let priority = |candidate| match candidate {
            ToolTruncationReason::TimeBudget => 6,
            ToolTruncationReason::BytesScanned => 5,
            ToolTruncationReason::FilesScanned => 4,
            ToolTruncationReason::EnumerationLimit => 4,
            ToolTruncationReason::ResultBytes => 3,
            ToolTruncationReason::LineTooLong => 3,
            ToolTruncationReason::MatchLimit => 2,
            ToolTruncationReason::EntryLimit => 1,
            ToolTruncationReason::PresentationReduced => 1,
        };
        if self
            .truncated_reason
            .is_none_or(|current| priority(reason) > priority(current))
        {
            self.truncated_reason = Some(reason);
        }
    }

    fn finish(self, footprint: Option<ReadFootprint>) -> SearchOutput {
        SearchOutput {
            preview: self.preview,
            preview_saturated: self.preview_saturated,
            match_count: self.match_count,
            max_matches: self.max_matches,
            total_bytes: self.total_bytes,
            complete: self.complete,
            footprint,
            structured: self.structured,
            truncated_reason: self.truncated_reason,
            binary_files_skipped: self.binary_files_skipped,
            skipped_sensitive: self.skipped_sensitive,
            files_scanned: self.files_scanned,
            bytes_scanned: self.bytes_scanned,
        }
    }
}

fn structured_match_bytes(found: &FsSearchMatch) -> ToolResult<usize> {
    serde_json::to_vec(found)
        .map(|encoded| encoded.len())
        .map_err(|error| ToolError::Runtime {
            message: format!("serialize structured fs_search match: {error}"),
        })
}

struct CompiledSearch {
    regex: Option<[Regex; 4]>,
    case_sensitive: bool,
}

impl CompiledSearch {
    fn new(operation: &FsSearch) -> ToolResult<Self> {
        let case_sensitive = match operation.case_mode {
            FsCaseMode::Sensitive => true,
            FsCaseMode::Insensitive => false,
            FsCaseMode::Smart => operation.query.chars().any(char::is_uppercase),
        };
        let regex_source = match operation.mode {
            FsSearchMode::Regex => Some(Cow::Borrowed(operation.query.as_str())),
            FsSearchMode::Simple => Some(Cow::Owned(simple_regex_pattern(&operation.query))),
            FsSearchMode::Literal => None,
        };
        let regex = if let Some(regex_source) = regex_source {
            Some(compile_boundary_regexes(
                &regex_source,
                case_sensitive,
                operation.mode == FsSearchMode::Regex && operation.multiline,
                operation.mode,
            )?)
        } else {
            None
        };
        Ok(Self {
            regex,
            case_sensitive,
        })
    }

    fn columns(
        &self,
        line: &str,
        operation: &FsSearch,
        limit: usize,
        line_number: usize,
        is_last_line: bool,
    ) -> Vec<usize> {
        match operation.mode {
            FsSearchMode::Regex => self.regex.as_ref().map_or_else(Vec::new, |regex| {
                regex_columns(
                    &regex[boundary_regex_index(line_number == 1, is_last_line)],
                    line,
                    limit,
                )
            }),
            FsSearchMode::Literal => {
                let (haystack, needle) = if self.case_sensitive {
                    (line.to_owned(), operation.query.clone())
                } else {
                    (line.to_lowercase(), operation.query.to_lowercase())
                };
                haystack.find(&needle).map_or_else(Vec::new, |column| {
                    vec![haystack[..column].chars().count().saturating_add(1)]
                })
            }
            FsSearchMode::Simple => {
                if self
                    .regex
                    .as_ref()
                    .is_some_and(|regex| regex[0].is_match(line))
                {
                    vec![1]
                } else {
                    Vec::new()
                }
            }
        }
    }
}

fn compile_boundary_regexes(
    source: &str,
    case_sensitive: bool,
    multiline: bool,
    mode: FsSearchMode,
) -> ToolResult<[Regex; 4]> {
    let error = |error: &dyn std::fmt::Display| ToolError::InvalidArgument {
        message: if mode == FsSearchMode::Regex {
            format!("invalid fs_search regex: {error}")
        } else {
            format!("invalid fs_search simple pattern: {error}")
        },
    };
    let hir = RegexParserBuilder::new()
        .nest_limit(SEARCH_REGEX_NEST_LIMIT)
        .multi_line(multiline)
        .case_insensitive(!case_sensitive)
        .build()
        .parse(source)
        .map_err(|compile_error| error(&compile_error))?;
    let mut compiled = Vec::with_capacity(4);
    for first_line in [false, true] {
        for last_line in [false, true] {
            let bounded_source = rewrite_file_boundaries(&hir, first_line, last_line).to_string();
            compiled.push(
                RegexBuilder::new(&bounded_source)
                    .size_limit(SEARCH_REGEX_SIZE_LIMIT)
                    .dfa_size_limit(SEARCH_REGEX_DFA_SIZE_LIMIT)
                    .nest_limit(SEARCH_REGEX_NEST_LIMIT)
                    .build()
                    .map_err(|compile_error| error(&compile_error))?,
            );
        }
    }
    compiled.try_into().map_err(|_| ToolError::Runtime {
        message: "fs_search did not construct all boundary regex variants".into(),
    })
}

fn rewrite_file_boundaries(hir: &Hir, first_line: bool, last_line: bool) -> Hir {
    match hir.kind() {
        HirKind::Empty => Hir::empty(),
        HirKind::Literal(literal) => Hir::literal(literal.0.clone()),
        HirKind::Class(class) => Hir::class(class.clone()),
        HirKind::Look(look) => match look {
            Look::Start => {
                if first_line {
                    Hir::look(Look::Start)
                } else {
                    Hir::fail()
                }
            }
            Look::End => {
                if last_line {
                    Hir::look(Look::End)
                } else {
                    Hir::fail()
                }
            }
            Look::StartLF | Look::StartCRLF => Hir::look(Look::Start),
            Look::EndLF | Look::EndCRLF => Hir::look(Look::End),
            other => Hir::look(*other),
        },
        HirKind::Repetition(repetition) => Hir::repetition(repetition.with(
            rewrite_file_boundaries(&repetition.sub, first_line, last_line),
        )),
        HirKind::Capture(capture) => Hir::capture(Capture {
            index: capture.index,
            name: capture.name.clone(),
            sub: Box::new(rewrite_file_boundaries(&capture.sub, first_line, last_line)),
        }),
        HirKind::Concat(expressions) => Hir::concat(
            expressions
                .iter()
                .map(|expression| rewrite_file_boundaries(expression, first_line, last_line))
                .collect(),
        ),
        HirKind::Alternation(expressions) => Hir::alternation(
            expressions
                .iter()
                .map(|expression| rewrite_file_boundaries(expression, first_line, last_line))
                .collect(),
        ),
    }
}

const fn boundary_regex_index(first_line: bool, last_line: bool) -> usize {
    (first_line as usize) * 2 + last_line as usize
}

fn simple_regex_pattern(pattern: &str) -> String {
    let mut translated = String::with_capacity(pattern.len());
    let mut characters = pattern.chars();
    while let Some(character) = characters.next() {
        match character {
            '*' => translated.push_str(".*"),
            '?' => translated.push('.'),
            '\\' => {
                if let Some(literal) = characters.next() {
                    push_escaped_regex_char(&mut translated, literal);
                } else {
                    translated.push_str(r"\\");
                }
            }
            literal => push_escaped_regex_char(&mut translated, literal),
        }
    }
    translated
}

fn push_escaped_regex_char(output: &mut String, character: char) {
    let mut encoded = [0_u8; 4];
    output.push_str(&regex::escape(character.encode_utf8(&mut encoded)));
}

fn regex_columns(regex: &Regex, line: &str, limit: usize) -> Vec<usize> {
    regex
        .find_iter(line)
        .take(limit)
        .map(|found| line[..found.start()].chars().count().saturating_add(1))
        .collect()
}

fn validate_search(operation: &FsSearch) -> ToolResult<()> {
    if operation.query.is_empty() {
        return Err(ToolError::invalid_argument(
            "fs_search query cannot be empty",
        ));
    }
    if operation.query.len() > SEARCH_PATTERN_MAX_BYTES {
        return Err(ToolError::invalid_argument(format!(
            "fs_search pattern cannot exceed {SEARCH_PATTERN_MAX_BYTES} UTF-8 bytes",
        )));
    }
    if operation.mode == FsSearchMode::Regex
        && operation.query.len() > SEARCH_REGEX_PATTERN_MAX_BYTES
    {
        return Err(ToolError::invalid_argument(format!(
            "fs_search regex cannot exceed {SEARCH_REGEX_PATTERN_MAX_BYTES} UTF-8 bytes",
        )));
    }
    if operation.mode == FsSearchMode::Simple
        && operation.query.len() > SEARCH_SIMPLE_PATTERN_MAX_BYTES
    {
        return Err(ToolError::invalid_argument(format!(
            "fs_search simple pattern cannot exceed {SEARCH_SIMPLE_PATTERN_MAX_BYTES} UTF-8 bytes",
        )));
    }
    if operation.context.before > 5 || operation.context.after > 5 {
        return Err(ToolError::invalid_argument(
            "fs_search context before/after cannot exceed 5",
        ));
    }
    if operation.max_matches == 0 || operation.max_matches > SEARCH_PREVIEW_MATCHES {
        return Err(ToolError::invalid_argument(format!(
            "fs_search max_matches must be between 1 and {SEARCH_PREVIEW_MATCHES}",
        )));
    }
    if operation.file_glob.include.len() > 32 || operation.file_glob.exclude.len() > 32 {
        return Err(ToolError::invalid_argument(
            "fs_search file_glob include/exclude cannot exceed 32 patterns",
        ));
    }
    if operation
        .file_glob
        .include
        .iter()
        .chain(&operation.file_glob.exclude)
        .any(String::is_empty)
    {
        return Err(ToolError::invalid_argument(
            "fs_search file_glob patterns cannot be empty",
        ));
    }
    Ok(())
}

struct CompiledPathFilters {
    legacy: Option<GlobMatcher>,
    include: Vec<GlobMatcher>,
    exclude: Vec<GlobMatcher>,
}

impl CompiledPathFilters {
    fn new(operation: &FsSearch) -> ToolResult<Self> {
        let legacy = operation
            .glob
            .as_deref()
            .map(|pattern| compile_path_glob(pattern, "fs_search glob"))
            .transpose()?;
        let include = operation
            .file_glob
            .include
            .iter()
            .map(|pattern| compile_path_glob(pattern, "fs_search file_glob.include"))
            .collect::<ToolResult<Vec<_>>>()?;
        let exclude = operation
            .file_glob
            .exclude
            .iter()
            .map(|pattern| compile_path_glob(pattern, "fs_search file_glob.exclude"))
            .collect::<ToolResult<Vec<_>>>()?;
        Ok(Self {
            legacy,
            include,
            exclude,
        })
    }

    fn matches(&self, path: &str) -> bool {
        self.legacy
            .as_ref()
            .is_none_or(|matcher| matcher.is_match(path))
            && (self.include.is_empty()
                || self.include.iter().any(|matcher| matcher.is_match(path)))
            && !self.exclude.iter().any(|matcher| matcher.is_match(path))
    }
}

fn compile_path_glob(pattern: &str, argument: &str) -> ToolResult<GlobMatcher> {
    if pattern.is_empty() || pattern.len() > GLOB_PATTERN_MAX_BYTES {
        return Err(ToolError::invalid_argument(format!(
            "{argument} must contain between 1 and {GLOB_PATTERN_MAX_BYTES} UTF-8 bytes",
        )));
    }
    let mut builder = GlobBuilder::new(pattern);
    builder.literal_separator(true).backslash_escape(true);
    builder
        .build()
        .map(|glob| glob.compile_matcher())
        .map_err(|error| ToolError::InvalidArgument {
            message: format!("invalid {argument} pattern: {error}"),
        })
}

fn portable_relative_path(path: &Path) -> ToolResult<String> {
    Ok(path_argument(path)?.replace('\\', "/"))
}

fn path_under_search_root<'a>(
    workspace_root: &Path,
    search_root: &Path,
    workspace_path: &'a Path,
) -> ToolResult<&'a Path> {
    let relative =
        workspace_path
            .strip_prefix(search_root)
            .map_err(|_| ToolError::WorkspaceBoundary {
                workspace_root: workspace_root.to_path_buf(),
                requested_path: workspace_path.to_path_buf(),
                resolved_path: Some(workspace_root.join(workspace_path)),
            })?;
    if relative.as_os_str().is_empty() {
        return workspace_path.file_name().map(Path::new).ok_or_else(|| {
            ToolError::InvalidArgument {
                message: format!("search root has no file name: {}", workspace_path.display()),
            }
        });
    }
    Ok(relative)
}

fn repository_read_footprint(
    workspace_dir: &OwnedFd,
    directories: &[PathBuf],
    files: &[PathBuf],
) -> Option<ReadFootprint> {
    let mut footprint = ReadFootprintBuilder::new();
    for path in directories.iter().chain(files) {
        if !footprint.is_cacheable() {
            return None;
        }
        let stamp = freshness_stamp_at(workspace_dir, path)?;
        footprint.push(stamp);
    }
    footprint.finish()
}

#[cfg(unix)]
fn freshness_stamp_at(workspace_dir: &OwnedFd, path: &Path) -> Option<FreshnessStamp> {
    let root = rustix::io::dup(workspace_dir).ok()?;
    let metadata = freshness_stat_at(root, path).ok()?;
    Some(FreshnessStamp {
        path: path.to_path_buf(),
        metadata: FreshnessMetadata::from_stat(&metadata),
    })
}

#[cfg(windows)]
fn freshness_stamp_at(workspace_dir: &OwnedFd, path: &Path) -> Option<FreshnessStamp> {
    let (_parent, _target, entry) = windows_anchored_entry(workspace_dir, path, path).ok()?;
    Some(FreshnessStamp {
        path: path.to_path_buf(),
        metadata: FreshnessMetadata(entry.identity),
    })
}

#[cfg(unix)]
fn open_search_file(
    workspace_dir: &OwnedFd,
    relative: &Path,
    display_path: &Path,
) -> ToolResult<fs::File> {
    let root = rustix::io::dup(workspace_dir)
        .map_err(|error| ToolError::io("duplicate workspace root", display_path, error))?;
    let file = open_target_at(
        root,
        relative,
        OFlags::RDONLY | OFlags::NONBLOCK,
        "open search file",
        display_path,
    )?;
    let metadata = rustix::fs::fstat(&file)
        .map_err(|error| ToolError::io("inspect search file", display_path, error))?;
    if FileType::from_raw_mode(metadata.st_mode) != FileType::RegularFile {
        return Err(ToolError::PathChanged {
            path: display_path.to_path_buf(),
            message: "enumerated search file is no longer a regular file".into(),
        });
    }
    Ok(fs::File::from(file))
}

#[cfg(windows)]
fn open_search_file(
    workspace_dir: &OwnedFd,
    relative: &Path,
    display_path: &Path,
) -> ToolResult<fs::File> {
    let (_parent, _target, entry) = windows_anchored_entry(workspace_dir, relative, display_path)?;
    if entry.identity.directory {
        return Err(ToolError::PathChanged {
            path: display_path.to_path_buf(),
            message: "enumerated search file became a directory".into(),
        });
    }
    Ok(entry.handle)
}

struct PendingContext {
    match_index: usize,
    remaining: usize,
}

fn collect_streamed_file_matches(
    file: fs::File,
    display_path: &Path,
    entry_path: &Path,
    operation: &FsSearch,
    compiled: &CompiledSearch,
    started: Instant,
    matches: &mut SearchCollector,
) -> ToolResult<()> {
    let remaining = SEARCH_MAX_SCANNED_BYTES.saturating_sub(matches.bytes_scanned);
    if remaining == 0 {
        matches.truncate(ToolTruncationReason::BytesScanned);
        return Ok(());
    }
    let mut file = file;
    let sniff_limit = remaining.min(SEARCH_BINARY_SNIFF_BYTES);
    let mut sniff = Vec::with_capacity(sniff_limit);
    Read::by_ref(&mut file)
        .take(sniff_limit as u64)
        .read_to_end(&mut sniff)
        .map_err(|error| ToolError::io("sniff search file", entry_path, error))?;
    if started.elapsed() >= SEARCH_WALL_TIME_BUDGET {
        matches.add_bytes_scanned(sniff.len());
        matches.truncate(ToolTruncationReason::TimeBudget);
        return Ok(());
    }
    if sniff.contains(&0) {
        matches.add_bytes_scanned(sniff.len());
        matches.skip_binary();
        if matches.bytes_scanned == SEARCH_MAX_SCANNED_BYTES
            && file
                .metadata()
                .is_ok_and(|metadata| metadata.len() > sniff.len() as u64)
        {
            matches.truncate(ToolTruncationReason::BytesScanned);
        }
        return Ok(());
    }
    let chained = Cursor::new(sniff).chain(file);
    let mut reader = BufReader::with_capacity(SEARCH_BINARY_SNIFF_BYTES, chained);

    let mut before = VecDeque::<String>::new();
    let mut pending = Vec::<PendingContext>::new();
    let mut buffer = Vec::new();
    let mut line_number = 0usize;
    let mut private_key = false;
    loop {
        if started.elapsed() >= SEARCH_WALL_TIME_BUDGET {
            matches.truncate(ToolTruncationReason::TimeBudget);
            break;
        }
        let remaining = SEARCH_MAX_SCANNED_BYTES.saturating_sub(matches.bytes_scanned);
        if remaining == 0 {
            matches.truncate(ToolTruncationReason::BytesScanned);
            break;
        }
        let read = read_bounded_search_line(
            &mut reader,
            &mut buffer,
            remaining,
            started + SEARCH_WALL_TIME_BUDGET,
        )
        .map_err(|error| ToolError::io("read search file", entry_path, error))?;
        matches.add_bytes_scanned(read.bytes_scanned);
        if read.time_budget_reached {
            matches.truncate(ToolTruncationReason::TimeBudget);
            break;
        }
        if read.eof && read.bytes_scanned == 0 {
            break;
        }
        line_number = line_number.saturating_add(1);
        if read.budget_exhausted {
            matches.truncate(ToolTruncationReason::BytesScanned);
            break;
        }
        if read.overlong {
            matches.truncate(ToolTruncationReason::LineTooLong);
            break;
        }
        if buffer.last() == Some(&b'\n') {
            buffer.pop();
        }
        if buffer.last() == Some(&b'\r') {
            buffer.pop();
        }
        let Ok(line) = std::str::from_utf8(&buffer) else {
            matches.skip_binary();
            break;
        };
        let redacted = crate::redact::redact_line_with_private_key_state(line, &mut private_key);
        let structured_line = utf8_prefix(&redacted.text, SEARCH_STRUCTURED_LINE_BYTES).to_owned();
        for context in &mut pending {
            matches.append_context_after(context.match_index, &structured_line)?;
            context.remaining = context.remaining.saturating_sub(1);
        }
        pending.retain(|context| context.remaining > 0);

        let is_last_line = read.eof
            || reader
                .fill_buf()
                .map_err(|error| ToolError::io("peek search file", entry_path, error))?
                .is_empty();
        let remaining_matches = operation
            .max_matches
            .saturating_sub(matches.match_count)
            .saturating_add(1);
        let columns = compiled.columns(
            line,
            operation,
            remaining_matches,
            line_number,
            is_last_line,
        );
        if started.elapsed() >= SEARCH_WALL_TIME_BUDGET {
            matches.truncate(ToolTruncationReason::TimeBudget);
            break;
        }
        if !columns.is_empty() {
            let display = portable_relative_path(display_path)?;
            let raw_legacy = format!("{display}:{line_number}:{line}");
            let preview_legacy = format!("{display}:{line_number}:{}", redacted.text);
            let structured = columns
                .into_iter()
                .map(|column| FsSearchMatch {
                    path: display.clone(),
                    line: line_number,
                    column,
                    text: structured_line.clone(),
                    context_before: before.iter().cloned().collect(),
                    context_after: Vec::new(),
                })
                .collect();
            let indices = matches.push_line(&raw_legacy, &preview_legacy, structured)?;
            if operation.context.after > 0 {
                pending.extend(indices.into_iter().map(|match_index| PendingContext {
                    match_index,
                    remaining: operation.context.after,
                }));
            }
        }
        before.push_back(structured_line);
        while before.len() > operation.context.before {
            before.pop_front();
        }
        if matches.truncated_reason == Some(ToolTruncationReason::ResultBytes)
            || matches.truncated_reason == Some(ToolTruncationReason::MatchLimit)
        {
            break;
        }
        if read.eof {
            break;
        }
    }
    Ok(())
}

struct SearchLineRead {
    bytes_scanned: usize,
    eof: bool,
    overlong: bool,
    budget_exhausted: bool,
    time_budget_reached: bool,
}

fn read_bounded_search_line<R: BufRead>(
    reader: &mut R,
    output: &mut Vec<u8>,
    budget: usize,
    deadline: Instant,
) -> std::io::Result<SearchLineRead> {
    output.clear();
    let mut scanned = 0usize;
    let mut overlong = false;
    loop {
        if Instant::now() >= deadline {
            return Ok(SearchLineRead {
                bytes_scanned: scanned,
                eof: false,
                overlong,
                budget_exhausted: false,
                time_budget_reached: true,
            });
        }
        let available = reader.fill_buf()?;
        if Instant::now() >= deadline {
            return Ok(SearchLineRead {
                bytes_scanned: scanned,
                eof: false,
                overlong,
                budget_exhausted: false,
                time_budget_reached: true,
            });
        }
        if available.is_empty() {
            return Ok(SearchLineRead {
                bytes_scanned: scanned,
                eof: true,
                overlong,
                budget_exhausted: false,
                time_budget_reached: false,
            });
        }
        let segment = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index.saturating_add(1));
        let allowed = segment.min(budget.saturating_sub(scanned));
        let retained = allowed.min(SEARCH_MAX_LINE_BYTES.saturating_sub(output.len()));
        output.extend_from_slice(&available[..retained]);
        overlong |= retained < allowed;
        let line_complete =
            allowed == segment && available.get(segment.saturating_sub(1)) == Some(&b'\n');
        reader.consume(allowed);
        scanned = scanned.saturating_add(allowed);
        if allowed < segment {
            return Ok(SearchLineRead {
                bytes_scanned: scanned,
                eof: false,
                overlong,
                budget_exhausted: true,
                time_budget_reached: false,
            });
        }
        if line_complete {
            let budget_exhausted = scanned == budget && !reader.fill_buf()?.is_empty();
            return Ok(SearchLineRead {
                bytes_scanned: scanned,
                eof: false,
                overlong,
                budget_exhausted,
                time_budget_reached: false,
            });
        }
        if scanned == budget {
            let eof = reader.fill_buf()?.is_empty();
            return Ok(SearchLineRead {
                bytes_scanned: scanned,
                eof,
                overlong,
                budget_exhausted: !eof,
                time_budget_reached: false,
            });
        }
    }
}

struct CappedOutput {
    contents: String,
    preview: String,
    truncated: bool,
    footprint: Option<ReadFootprint>,
    truncated_reason: Option<ToolTruncationReason>,
    skipped_sensitive: usize,
    files_scanned: usize,
    collapsed_directories: usize,
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
    workspace_root: &Path,
    relative: &Path,
    operation: &FsGlob,
) -> ToolResult<CappedOutput> {
    if operation.pattern.is_empty() {
        return Err(ToolError::invalid_argument(
            "fs_glob pattern cannot be empty",
        ));
    }
    let pattern = compile_path_glob(&operation.pattern, "fs_glob")?;
    let walked = crate::repo::walk_files(
        workspace_root,
        &operation.root,
        crate::repo::WalkOptions {
            respect_gitignore: operation.respect_gitignore,
            include_hidden: operation.include_hidden,
            max_files: GLOB_MAX_FILES_SCANNED,
            deadline: None,
        },
    )?;
    let footprint = if !operation.respect_gitignore && !walked.truncated {
        repository_read_footprint(&workspace_dir, &walked.directories, &walked.files)
    } else {
        None
    };
    let mut paths = GlobCollector::new();
    let mut skipped_sensitive = walked
        .hidden_sensitive_files
        .iter()
        .filter_map(|path| path_under_search_root(workspace_root, relative, path).ok())
        .filter_map(|path| portable_relative_path(path).ok())
        .filter(|path| pattern.is_match(path))
        .count();
    let mut files_scanned = 0usize;
    for workspace_path in walked.files {
        files_scanned = files_scanned.saturating_add(1);
        let path_under_root = path_under_search_root(workspace_root, relative, &workspace_path)?;
        let candidate = portable_relative_path(path_under_root)?;
        if !pattern.is_match(&candidate) {
            continue;
        }
        if crate::redact::is_sensitive_path(&workspace_path) {
            skipped_sensitive = skipped_sensitive.saturating_add(1);
            continue;
        }
        let display_path = workspace_root.join(&workspace_path);
        let file = open_search_file(&workspace_dir, &workspace_path, &display_path)?;
        drop(file);
        paths.push(portable_relative_path(&workspace_path)?);
    }
    let truncated = paths.truncated;
    let entries = paths.entries.into_sorted_vec();
    let (preview, collapsed_directories) = glob_preview(&entries);
    Ok(CappedOutput {
        contents: join_lines(entries),
        preview,
        truncated: truncated || walked.truncated || collapsed_directories > 0,
        footprint,
        truncated_reason: if walked.truncated {
            Some(ToolTruncationReason::EnumerationLimit)
        } else if truncated {
            Some(ToolTruncationReason::EntryLimit)
        } else if collapsed_directories > 0 {
            Some(ToolTruncationReason::PresentationReduced)
        } else {
            None
        },
        skipped_sensitive,
        files_scanned,
        collapsed_directories,
    })
}

fn glob_preview(entries: &[String]) -> (String, usize) {
    let mut vendor_counts = HashMap::<String, usize>::new();
    let mut extension_counts = HashMap::<(String, String), usize>::new();
    for path in entries {
        if let Some(vendor) = ["node_modules", "target", "vendor", ".venv"]
            .into_iter()
            .find(|vendor| path.split('/').any(|part| part == *vendor))
        {
            *vendor_counts.entry(vendor.to_owned()).or_insert(0) += 1;
            continue;
        }
        let parent = Path::new(path)
            .parent()
            .and_then(Path::to_str)
            .unwrap_or("")
            .to_owned();
        let extension = Path::new(path)
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        *extension_counts.entry((parent, extension)).or_insert(0) += 1;
    }
    let mut extension_seen = HashMap::<(String, String), usize>::new();
    let mut preview = Vec::new();
    let mut collapsed = vendor_counts.values().copied().sum::<usize>();
    for path in entries {
        if ["node_modules", "target", "vendor", ".venv"]
            .into_iter()
            .any(|vendor| path.split('/').any(|part| part == vendor))
        {
            continue;
        }
        let parent = Path::new(path)
            .parent()
            .and_then(Path::to_str)
            .unwrap_or("")
            .to_owned();
        let extension = Path::new(path)
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let key = (parent.clone(), extension.clone());
        let count = extension_counts.get(&key).copied().unwrap_or(0);
        let seen = extension_seen.entry(key).or_insert(0);
        *seen = seen.saturating_add(1);
        if count >= DIRECTORY_EXTENSION_COLLAPSE_THRESHOLD && *seen > DIRECTORY_EXTENSION_EXAMPLES {
            if *seen == DIRECTORY_EXTENSION_EXAMPLES + 1 {
                let prefix = if parent.is_empty() {
                    String::new()
                } else {
                    format!("{parent}/")
                };
                let label = if extension.is_empty() {
                    "files".to_owned()
                } else {
                    format!(".{extension} files")
                };
                preview.push(format!(
                    "{prefix}[… {} more {label}]",
                    count.saturating_sub(DIRECTORY_EXTENSION_EXAMPLES)
                ));
            }
            collapsed = collapsed.saturating_add(1);
            continue;
        }
        preview.push(path.clone());
    }
    let mut vendors = vendor_counts.into_iter().collect::<Vec<_>>();
    vendors.sort_by(|left, right| left.0.cmp(&right.0));
    preview.extend(
        vendors
            .into_iter()
            .map(|(vendor, count)| format!("{vendor}/: {count} paths collapsed")),
    );
    (join_lines(preview), collapsed)
}

#[cfg(test)]
fn wildcard_matches(pattern: &str, text: &str, slash_sensitive: bool) -> bool {
    let pattern = pattern.chars().collect::<Vec<_>>();
    let text = text.chars().collect::<Vec<_>>();
    // The old recursive matcher memoized these states, which bounded the total
    // work but not the call depth: a leading `*` still added one stack frame
    // per text character. Keep both the pending and visited states on the heap
    // so long minified lines cannot exhaust a worker's small native stack.
    let mut pending = vec![(0, 0)];
    let mut visited = HashSet::new();
    while let Some((pattern_index, text_index)) = pending.pop() {
        if !visited.insert((pattern_index, text_index)) {
            continue;
        }
        match pattern.get(pattern_index) {
            None => {
                if text_index == text.len() {
                    return true;
                }
            }
            Some('*') => {
                let double = pattern.get(pattern_index + 1) == Some(&'*');
                let next_pattern = pattern_index + if double { 2 } else { 1 };
                if text
                    .get(text_index)
                    .is_some_and(|character| double || !slash_sensitive || *character != '/')
                {
                    pending.push((pattern_index, text_index + 1));
                }
                pending.push((next_pattern, text_index));
                if double && pattern.get(next_pattern) == Some(&'/') {
                    pending.push((next_pattern + 1, text_index));
                }
            }
            Some('?') => {
                if text
                    .get(text_index)
                    .is_some_and(|character| !slash_sensitive || *character != '/')
                {
                    pending.push((pattern_index + 1, text_index + 1));
                }
            }
            Some('\\') if pattern.get(pattern_index + 1).is_some() => {
                if text.get(text_index) == pattern.get(pattern_index + 1) {
                    pending.push((pattern_index + 2, text_index + 1));
                }
            }
            Some(expected) => {
                if text.get(text_index) == Some(expected) {
                    pending.push((pattern_index + 1, text_index + 1));
                }
            }
        }
    }
    false
}

#[cfg(all(test, unix))]
#[allow(clippy::expect_used)]
mod read_memo_tests {
    use super::*;
    use crate::broker::JournalSink;
    use crate::ledger::ChangeLedger;
    use haider_protocol::EventPayload;
    use haider_protocol::ids::{ArtifactRef, RunId};

    struct RecordingJournal;

    #[async_trait::async_trait]
    impl JournalSink for RecordingJournal {
        async fn append(&mut self, _payload: EventPayload) -> ToolResult<()> {
            Ok(())
        }

        fn supports_checkpoint_batches(&self) -> bool {
            true
        }

        fn supports_checkpoint_artifacts(&self) -> bool {
            true
        }

        async fn put_checkpoint_artifact(&mut self, bytes: &[u8]) -> ToolResult<ArtifactRef> {
            Ok(ArtifactRef::new(format!(
                "blake3:{}",
                blake3::hash(bytes).to_hex()
            )))
        }

        async fn append_checkpointed(
            &mut self,
            _outcome: EventPayload,
            _checkpoint: EventPayload,
        ) -> ToolResult<()> {
            Ok(())
        }
    }

    struct WorkingCas;

    #[async_trait::async_trait]
    impl CasSink for WorkingCas {
        async fn put(&mut self, bytes: &[u8]) -> ToolResult<ArtifactRef> {
            Ok(ArtifactRef::new(format!(
                "blake3:{}",
                blake3::hash(bytes).to_hex()
            )))
        }

        async fn put_file(&mut self, path: &Path) -> ToolResult<ArtifactRef> {
            let bytes = fs::read(path)
                .map_err(|error| ToolError::cas(format!("read test CAS input: {error}")))?;
            self.put(&bytes).await
        }
    }

    struct RefusingCas;

    #[async_trait::async_trait]
    impl CasSink for RefusingCas {
        async fn put(&mut self, _bytes: &[u8]) -> ToolResult<ArtifactRef> {
            Err(ToolError::cas("memo miss reached the refusing CAS"))
        }

        async fn put_file(&mut self, _path: &Path) -> ToolResult<ArtifactRef> {
            Err(ToolError::cas("memo miss reached the refusing CAS"))
        }
    }

    fn broker(root: &Path, session: &str) -> EffectBroker {
        EffectBroker::new_at(
            Box::new(RecordingJournal),
            root,
            SessionId::new(session),
            1,
            1_900_000_000_000,
        )
        .expect("memo test broker")
    }

    fn allow(class: EffectClass) -> PermissionPolicy {
        let mut policy = PermissionPolicy::default();
        policy.allow(class);
        policy
    }

    #[tokio::test]
    async fn repeated_read_and_search_reuse_results_until_a_workspace_mutation() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        let large_file = "read payload ".repeat(128);
        fs::write(directory.path().join("read.txt"), &large_file).expect("seed read input");
        let search_file = (0..64)
            .map(|index| format!("needle result {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(directory.path().join("search.txt"), search_file).expect("seed search input");
        let mut broker = broker(directory.path(), "memo-repeat");
        let bounds = ResultBounds {
            max_preview_bytes: 64,
        };

        let first_read = broker
            .fs_read(
                &FsRead::new("read.txt"),
                &allow(EffectClass::FsRead),
                &mut WorkingCas,
                bounds,
            )
            .await
            .expect("first read populates memo");
        let repeated_read = broker
            .fs_read(
                &FsRead::new("read.txt"),
                &allow(EffectClass::FsRead),
                &mut RefusingCas,
                bounds,
            )
            .await
            .expect("unchanged read bypasses refusing CAS");
        assert_eq!(repeated_read, first_read);

        let search = FsSearch::new(".", "needle").with_repo_options(false, false);
        let first_search = broker
            .fs_search(
                &search,
                &allow(EffectClass::FsRead),
                &mut WorkingCas,
                bounds,
            )
            .await
            .expect("first search populates memo");
        let repeated_search = broker
            .fs_search(
                &search,
                &allow(EffectClass::FsRead),
                &mut RefusingCas,
                bounds,
            )
            .await
            .expect("unchanged search bypasses refusing CAS");
        assert_eq!(repeated_search, first_search);

        broker
            .fs_write(
                &FsWrite::new("mutation.txt", "changed"),
                &allow(EffectClass::FsWrite),
                &TurnAttribution::new(SessionId::new("memo-repeat"), RunId::new("turn")),
                &ChangeLedger::new(),
            )
            .await
            .expect("workspace mutation");
        let error = broker
            .fs_read(
                &FsRead::new("read.txt"),
                &allow(EffectClass::FsRead),
                &mut RefusingCas,
                bounds,
            )
            .await
            .expect_err("mutation invalidates the prior read result");
        assert!(matches!(error, ToolError::Cas { .. }));
    }

    #[test]
    fn read_memo_lru_never_exceeds_its_byte_cap() {
        let cap = 2_048;
        let mut memo = ReadMemo::new(cap);
        let value = |byte: char| MemoizedRead {
            result: BoundedResult {
                preview: byte.to_string().repeat(900),
                truncated: true,
                data: None,
                artifact: Some(ArtifactRef::new(format!("blake3:{byte}"))),
                images: Vec::new(),
                cursor: None,
                status: haider_protocol::tool::ToolResultStatus::Completed,
                reason: None,
                presentation: None,
            },
            freshness: None,
            footprint: ReadFootprint::new(Vec::new()),
        };
        let key = |digest: &str| ReadMemoCallKey {
            scope: "session-scope".into(),
            workspace: PathBuf::from("workspace"),
            tool: "fs_read",
            args_digest: digest.into(),
            max_preview_bytes: 900,
        };
        let first = key("first");
        let second = key("second");

        memo.insert(first.clone(), value('a'));
        memo.insert(second.clone(), value('b'));

        assert!(memo.used_bytes <= cap);
        assert!(
            !memo.entries.contains_key(&first),
            "oldest entry is evicted"
        );
        assert!(memo.entries.contains_key(&second));
    }
}

#[cfg(test)]
mod wildcard_match_tests {
    use super::wildcard_matches;

    const SMALL_STACK_BYTES: usize = 256 * 1024;
    const LONG_LINE_CHARS: usize = 100_000;

    #[test]
    // Test fixture failures should retain whether thread creation or execution failed.
    #[allow(clippy::expect_used)]
    fn long_wildcard_search_survives_a_small_worker_stack() {
        std::thread::Builder::new()
            .name("wildcard-small-stack".into())
            .stack_size(SMALL_STACK_BYTES)
            .spawn(|| {
                let line = "x".repeat(LONG_LINE_CHARS);
                assert!(!wildcard_matches("*needle*", &line, false));
            })
            .expect("spawn small-stack matcher thread")
            .join()
            .expect("small-stack matcher thread survives");
    }

    #[test]
    fn long_wildcard_search_keeps_correct_results() {
        let matching = format!("{}needle{}", "x".repeat(LONG_LINE_CHARS), "y".repeat(1_000));
        let missing = "x".repeat(LONG_LINE_CHARS);

        assert!(wildcard_matches("*needle*", &matching, false));
        assert!(!wildcard_matches("*needle*", &missing, false));
    }
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
    let (_parent, target, entry) = windows_anchored_entry(&workspace_dir, relative, display_path)?;
    let metadata = entry
        .handle
        .metadata()
        .map_err(|error| ToolError::io("inspect", display_path, error))?;
    let footprint = ReadFootprint::new(vec![FreshnessStamp {
        path: relative.to_path_buf(),
        metadata: FreshnessMetadata(entry.identity),
    }]);
    if metadata.is_file() {
        let contents = read_utf8_file(entry.handle, display_path)?;
        let digest = format!("blake3:{}", blake3::hash(contents.as_bytes()).to_hex());
        let preview_contents = if offset.is_some() || limit.is_some() {
            Some(select_numbered_lines(
                &crate::redact::redact_private_key_lines(&contents).text,
                offset.unwrap_or(1),
                limit,
            ))
        } else {
            None
        };
        let contents = if offset.is_some() || limit.is_some() {
            select_numbered_lines(&contents, offset.unwrap_or(1), limit)
        } else {
            contents
        };
        Ok(ReadPathOutput {
            contents,
            preview_contents,
            digest: Some(digest),
            footprint,
            data: None,
        })
    } else if metadata.is_dir() {
        let entries = fs::read_dir(&target)
            .map_err(|error| ToolError::io("list directory", display_path, error))?;
        let mut collector = DirectoryCollector::new();
        for entry in entries {
            let entry =
                entry.map_err(|error| ToolError::io("list directory", display_path, error))?;
            let mut name = entry.file_name().to_string_lossy().into_owned();
            let is_directory = entry.file_type().is_ok_and(|kind| kind.is_dir());
            if is_directory {
                name.push('/');
            }
            collector.push(name, is_directory);
        }
        let listing = collector.finish();
        Ok(ReadPathOutput {
            contents: listing.contents,
            preview_contents: None,
            digest: None,
            footprint,
            data: Some(listing.data),
        })
    } else {
        Err(ToolError::invalid_argument(format!(
            "fs_read path is not a regular file or directory: {}",
            display_path.display()
        )))
    }
}

#[cfg(windows)]
fn windows_anchored_entry(
    workspace_root: &OwnedFd,
    relative: &Path,
    display_path: &Path,
) -> ToolResult<(OwnedFd, PathBuf, WindowsPathEntry)> {
    if relative.is_absolute()
        || relative.components().any(|component| {
            !matches!(
                component,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
    {
        return Err(ToolError::WorkspaceBoundary {
            workspace_root: workspace_root.path().to_path_buf(),
            requested_path: display_path.to_path_buf(),
            resolved_path: None,
        });
    }
    let (parent, candidate) = if relative.as_os_str().is_empty() {
        let parent = haider_platform::duplicate_workspace_directory(workspace_root)
            .map_err(|error| ToolError::io("duplicate workspace root", display_path, error))?;
        let candidate = parent.path().to_path_buf();
        (parent, candidate)
    } else {
        windows_mutation_target(workspace_root, relative, display_path, false)?
    };
    let entry = open_windows_path_entry(&candidate, display_path, false)?;
    Ok((parent, candidate, entry))
}

struct AppliedMutation {
    result: BoundedResult,
    paths: Vec<PathBuf>,
    post_digest: String,
    checkpoint: CheckpointCapture,
}

enum MutationWorkerOutcome {
    Applied {
        result: BoundedResult,
        effect: haider_protocol::ids::EffectId,
        post_digest: String,
        checkpoint: CheckpointCapture,
    },
    ApplyFailed(ToolError),
    PostApplyFailed {
        error: ToolError,
        effect: haider_protocol::ids::EffectId,
        post_digest: String,
        checkpoint: CheckpointCapture,
    },
}

impl MutationWorkerOutcome {
    fn into_result(
        self,
    ) -> (
        ToolResult<BoundedResult>,
        Option<WorkspaceMutation>,
        Option<CheckpointCapture>,
    ) {
        match self {
            Self::Applied {
                result,
                effect,
                post_digest,
                checkpoint,
            } => (
                Ok(result),
                Some(workspace_mutation(effect, post_digest)),
                Some(checkpoint),
            ),
            Self::ApplyFailed(error) => (Err(error), None, None),
            Self::PostApplyFailed {
                error,
                effect,
                post_digest,
                checkpoint,
            } => (
                Err(error),
                Some(workspace_mutation(effect, post_digest)),
                Some(checkpoint),
            ),
        }
    }

    fn into_result_with_freshness(
        self,
        relative_path: String,
    ) -> (
        ToolResult<BoundedResult>,
        Option<FileFreshness>,
        Option<WorkspaceMutation>,
        Option<CheckpointCapture>,
    ) {
        match self {
            Self::Applied {
                result,
                effect,
                post_digest,
                checkpoint,
            } => {
                let mutation = workspace_mutation(effect, post_digest.clone());
                (
                    Ok(result),
                    Some(FileFreshness {
                        path: relative_path,
                        digest: post_digest,
                    }),
                    Some(mutation),
                    Some(checkpoint),
                )
            }
            Self::ApplyFailed(error) => (Err(error), None, None, None),
            Self::PostApplyFailed {
                error,
                effect,
                post_digest,
                checkpoint,
            } => {
                let mutation = workspace_mutation(effect, post_digest.clone());
                (
                    Err(error),
                    Some(FileFreshness {
                        path: relative_path,
                        digest: post_digest,
                    }),
                    Some(mutation),
                    Some(checkpoint),
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

fn sync_mutation_parents(paths: &[PathBuf]) -> ToolResult<()> {
    let mut parents = paths
        .iter()
        .map(|path| {
            path.parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or(Path::new("."))
                .to_path_buf()
        })
        .collect::<Vec<_>>();
    parents.sort();
    parents.dedup();
    for parent in parents {
        haider_platform::sync_directory(&parent)
            .map_err(|error| ToolError::io("sync mutation parent", &parent, error))?;
    }
    Ok(())
}

/// Installs one checkpoint state through the same anchored, no-follow commit
/// machinery as ordinary filesystem mutations. The relative name is resolved
/// from an open workspace-directory handle, so a swapped parent cannot
/// redirect the restore through a newly introduced symlink.
#[cfg(unix)]
pub(crate) fn install_checkpoint_state(
    workspace_root: &Path,
    relative: &Path,
    expected_digest: Option<&str>,
    bytes: Option<&[u8]>,
) -> Result<(), crate::checkpoint::InstallStateError> {
    use crate::checkpoint::InstallStateError;

    let display_path = workspace_root.join(relative);
    let workspace_dir = haider_platform::open_workspace_directory(workspace_root)
        .map_err(|error| ToolError::io("open checkpoint workspace", workspace_root, error))?;
    let traversal_root = rustix::io::dup(&workspace_dir)
        .map_err(|error| ToolError::io("duplicate checkpoint workspace", workspace_root, error))?;
    let (parent, leaf) = open_parent_at(traversal_root, relative, &display_path)?;
    let mut current = match rustix::fs::statat(&parent, &leaf, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => {
            let (mut file, metadata) = open_locked_current_at(&parent, &leaf, &display_path)
                .map_err(checkpoint_install_error)?;
            let (current_bytes, _) = file_snapshot(&parent, &mut file, &display_path)
                .map_err(checkpoint_install_error)?
                .parts();
            let current_digest = mutation_digest(&current_bytes);
            match expected_digest {
                Some(expected) if current_digest == expected => {}
                _ => {
                    return Err(InstallStateError::Conflict {
                        current_digest: Some(current_digest),
                    });
                }
            }
            Some((file, metadata, blake3::hash(&current_bytes)))
        }
        Err(rustix::io::Errno::NOENT) if expected_digest.is_none() => None,
        Err(rustix::io::Errno::NOENT) => {
            return Err(InstallStateError::Conflict {
                current_digest: None,
            });
        }
        Err(error) => {
            return Err(checkpoint_install_error(anchored_io_error(
                "inspect checkpoint target",
                &display_path,
                error,
            )));
        }
    };

    let staged = match bytes {
        Some(bytes) => {
            let (name, fd) = create_patch_temporary(&parent, &display_path)?;
            let mode = current
                .as_ref()
                .map_or(0o644, |(_, metadata, _)| metadata.st_mode);
            if let Err(error) = write_patch_temporary(fd, mode, bytes, &display_path) {
                remove_temporary(&parent, &name);
                return Err(error.into());
            }
            Some(name)
        }
        None => None,
    };
    let commit_parent =
        match revalidate_commit_parent(&workspace_dir, relative, &parent, &display_path) {
            Ok(parent) => parent,
            Err(error) => {
                if let Some(name) = staged.as_deref() {
                    remove_temporary(&parent, name);
                }
                return Err(checkpoint_install_error(error));
            }
        };
    if let Some((file, _, source_hash)) = current.as_mut()
        && let Err(error) = require_unchanged_content(&parent, file, *source_hash, &display_path)
    {
        if let Some(name) = staged.as_deref() {
            remove_temporary(&parent, name);
        }
        return Err(checkpoint_install_error(error));
    }
    if let Err(error) = require_unchanged_target(
        &commit_parent,
        &leaf,
        current.as_ref().map(|(_, metadata, _)| metadata),
        &display_path,
    ) {
        if let Some(name) = staged.as_deref() {
            remove_temporary(&parent, name);
        }
        return Err(checkpoint_install_error(error));
    }

    match (staged.as_deref(), current.is_some()) {
        (Some(name), true) => {
            if let Err(error) = replace_temporary_at_commit(
                &commit_parent,
                name,
                &leaf,
                &display_path,
                "publish checkpoint restore",
            ) {
                remove_temporary(&parent, name);
                return Err(checkpoint_install_error(error));
            }
        }
        (Some(name), false) => {
            if let Err(error) = rustix::fs::renameat_with(
                &commit_parent,
                name,
                &commit_parent,
                &leaf,
                rustix::fs::RenameFlags::NOREPLACE,
            ) {
                remove_temporary(&parent, name);
                return Err(if error == rustix::io::Errno::EXIST {
                    InstallStateError::Conflict {
                        current_digest: None,
                    }
                } else {
                    checkpoint_install_error(anchored_io_error(
                        "publish absent checkpoint restore",
                        &display_path,
                        error,
                    ))
                });
            }
        }
        (None, true) => {
            rustix::fs::unlinkat(&commit_parent, &leaf, AtFlags::empty()).map_err(|error| {
                checkpoint_install_error(anchored_io_error(
                    "remove checkpoint restore target",
                    &display_path,
                    error,
                ))
            })?
        }
        (None, false) => {}
    }
    sync_mutation_parents(&[display_path]).map_err(InstallStateError::Tool)
}

#[cfg(unix)]
fn checkpoint_install_error(error: ToolError) -> crate::checkpoint::InstallStateError {
    match error {
        ToolError::PathChanged { .. } | ToolError::StaleRead { .. } => {
            crate::checkpoint::InstallStateError::Conflict {
                current_digest: None,
            }
        }
        error => crate::checkpoint::InstallStateError::Tool(error),
    }
}

#[cfg(windows)]
pub(crate) fn install_checkpoint_state(
    workspace_root: &Path,
    relative: &Path,
    expected_digest: Option<&str>,
    bytes: Option<&[u8]>,
) -> Result<(), crate::checkpoint::InstallStateError> {
    use crate::checkpoint::InstallStateError;

    let display_path = workspace_root.join(relative);
    let workspace_dir = haider_platform::open_workspace_directory(workspace_root)
        .map_err(|error| ToolError::io("open checkpoint workspace", workspace_root, error))?;
    let (parent, target) = windows_mutation_target(&workspace_dir, relative, &display_path, false)?;
    let mut current = match fs::symlink_metadata(&target) {
        Ok(metadata) => {
            require_windows_regular_file(&target, &display_path, &metadata)
                .map_err(checkpoint_install_error)?;
            let mut file = open_windows_locked_file(&target, &display_path)
                .map_err(checkpoint_install_error)?;
            let snapshot = windows_stable_snapshot(&mut file, &display_path)
                .map_err(checkpoint_install_error)?;
            let current_digest = mutation_digest(&snapshot.bytes);
            match expected_digest {
                Some(expected) if expected == current_digest => {}
                _ => {
                    return Err(InstallStateError::Conflict {
                        current_digest: Some(current_digest),
                    });
                }
            }
            let permissions = file
                .metadata()
                .map_err(|error| {
                    ToolError::io("inspect checkpoint permissions", &display_path, error)
                })?
                .permissions();
            Some((
                file,
                snapshot.identity,
                blake3::hash(&snapshot.bytes),
                permissions,
            ))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && expected_digest.is_none() => {
            None
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(InstallStateError::Conflict {
                current_digest: None,
            });
        }
        Err(error) => {
            return Err(ToolError::io("inspect checkpoint target", &display_path, error).into());
        }
    };
    let staged = match bytes {
        Some(bytes) => {
            let staged = stage_windows_content(
                parent.path(),
                &display_path,
                bytes,
                current
                    .as_ref()
                    .map(|(_, _, _, permissions)| permissions.clone()),
            )?;
            if let Some((file, _, _, _)) = current.as_ref()
                && let Err(error) = copy_windows_dacl(file, &staged.file, &display_path)
            {
                let _ = delete_windows_entry(staged.file, &display_path);
                return Err(error.into());
            }
            Some(staged)
        }
        None => None,
    };
    let revalidated = match current.as_mut() {
        Some((file, identity, hash, _)) => revalidate_windows_mutation(
            &workspace_dir,
            relative,
            &parent,
            &target,
            &display_path,
            Some((file, *identity, *hash)),
        ),
        None => revalidate_windows_mutation(
            &workspace_dir,
            relative,
            &parent,
            &target,
            &display_path,
            None,
        ),
    };
    let revalidated = match revalidated {
        Ok(value) => value,
        Err(error) => {
            if let Some(staged) = staged {
                let _ = delete_windows_entry(staged.file, &display_path);
            }
            return Err(checkpoint_install_error(error));
        }
    };
    match (staged, current.as_ref()) {
        (Some(staged), current) => publish_windows_temporary(
            staged,
            &target,
            current.is_some(),
            blake3::hash(bytes.unwrap_or_default()),
            &display_path,
        )
        .map_err(checkpoint_install_error)?,
        (None, Some((_, expected_identity, _, _))) => {
            let entry = open_windows_path_entry(&target, &display_path, true)
                .map_err(checkpoint_install_error)?;
            if entry.identity.file != *expected_identity {
                return Err(InstallStateError::Conflict {
                    current_digest: None,
                });
            }
            remove_windows_entry_from_handle(&target, &display_path, entry)
                .map_err(checkpoint_install_error)?;
        }
        (None, None) => {}
    }
    drop(revalidated);
    drop(current);
    sync_mutation_parents(&[display_path]).map_err(InstallStateError::Tool)
}

#[cfg(windows)]
fn checkpoint_install_error(error: ToolError) -> crate::checkpoint::InstallStateError {
    match error {
        ToolError::PathChanged { .. } | ToolError::StaleRead { .. } => {
            crate::checkpoint::InstallStateError::Conflict {
                current_digest: None,
            }
        }
        error => crate::checkpoint::InstallStateError::Tool(error),
    }
}

fn checkpoint_freeze_input(
    attribution: &TurnAttribution,
    effect: &haider_protocol::ids::EffectId,
) -> FreezeCheckpointInput {
    FreezeCheckpointInput {
        session_id: attribution.session.clone(),
        branch_id: attribution.branch.clone(),
        run_id: attribution.turn.clone(),
        effect_id: effect.clone(),
        call_id: attribution.call_id.clone(),
        origin: CheckpointOrigin::Tool,
        source_checkpoint_id: None,
    }
}

#[cfg(windows)]
fn apply_write_and_record<L>(
    workspace_dir: OwnedFd,
    relative: &Path,
    operation: &FsWrite,
    context: MutationRecordContext<'_, L>,
) -> MutationWorkerOutcome
where
    L: ChangeLedgerSink,
{
    let applied =
        match apply_windows_write(&workspace_dir, relative, operation, context.expected_digest) {
            Ok(applied) => applied,
            Err(error) => return MutationWorkerOutcome::ApplyFailed(error),
        };
    record_windows_mutation(applied, context)
}

#[cfg(windows)]
fn apply_edit_and_record<L>(
    workspace_dir: OwnedFd,
    relative: &Path,
    operation: &FsEdit,
    context: MutationRecordContext<'_, L>,
) -> MutationWorkerOutcome
where
    L: ChangeLedgerSink,
{
    let applied =
        match apply_windows_edit(&workspace_dir, relative, operation, context.expected_digest) {
            Ok(applied) => applied,
            Err(error) => return MutationWorkerOutcome::ApplyFailed(error),
        };
    record_windows_mutation(applied, context)
}

#[cfg(windows)]
fn record_windows_mutation<L>(
    applied: AppliedMutation,
    context: MutationRecordContext<'_, L>,
) -> MutationWorkerOutcome
where
    L: ChangeLedgerSink,
{
    let AppliedMutation {
        result,
        paths,
        post_digest,
        checkpoint,
    } = applied;
    let effect = context.effect.clone();
    if let Err(error) = sync_mutation_parents(&paths) {
        return MutationWorkerOutcome::PostApplyFailed {
            error,
            effect,
            post_digest,
            checkpoint,
        };
    }
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
            checkpoint,
        },
        Err(error) => MutationWorkerOutcome::PostApplyFailed {
            error,
            effect,
            post_digest,
            checkpoint,
        },
    }
}

#[cfg(windows)]
// Keep the explicit Windows publication error branch so handle/drop ordering remains auditable.
#[allow(clippy::question_mark)]
fn apply_windows_write(
    workspace_root: &OwnedFd,
    relative: &Path,
    operation: &FsWrite,
    expected_digest: Option<&str>,
) -> ToolResult<AppliedMutation> {
    let (parent, target) =
        windows_mutation_target(workspace_root, relative, &operation.path, true)?;
    let mut existing = match fs::symlink_metadata(&target) {
        Ok(_) => {
            let mut file = open_windows_locked_file(&target, &operation.path)?;
            let source = windows_stable_snapshot(&mut file, &operation.path)?;
            let current_digest = mutation_digest(&source.bytes);
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
            Some(WindowsSource {
                _file: file,
                identity: source.identity,
                hash: blake3::hash(&source.bytes),
                bytes: source.bytes,
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(ToolError::io(
                "inspect write target",
                &operation.path,
                error,
            ));
        }
    };
    let bytes = operation.content.as_bytes();
    let permissions = existing
        .as_ref()
        .map(|source| {
            source
                ._file
                .metadata()
                .map(|metadata| metadata.permissions())
        })
        .transpose()
        .map_err(|error| ToolError::io("inspect target permissions", &operation.path, error))?;
    let temporary = stage_windows_content(parent.path(), &operation.path, bytes, permissions)?;
    if let Some(source) = existing.as_ref()
        && let Err(error) = copy_windows_dacl(&source._file, &temporary.file, &operation.path)
    {
        let _ = delete_windows_entry(temporary.file, &operation.path);
        return Err(error);
    }
    let revalidated = match revalidate_windows_mutation(
        workspace_root,
        relative,
        &parent,
        &target,
        &operation.path,
        existing
            .as_mut()
            .map(|source| (&mut source._file, source.identity, source.hash)),
    ) {
        Ok(revalidated) => revalidated,
        Err(error) => {
            let _ = delete_windows_entry(temporary.file, &operation.path);
            return Err(error);
        }
    };
    if let Err(error) = publish_windows_temporary(
        temporary,
        &target,
        existing.is_some(),
        blake3::hash(bytes),
        &operation.path,
    ) {
        return Err(error);
    }
    drop(revalidated);
    let pre_bytes = existing.take().map(|source| source.bytes);
    let checkpoint_kind = if pre_bytes.is_some() {
        CheckpointKind::Write
    } else {
        CheckpointKind::Create
    };
    let post_digest = mutation_digest(bytes);
    Ok(AppliedMutation {
        result: BoundedResult {
            preview: format!(
                "wrote {} bytes to {}",
                bytes.len(),
                operation.path.display()
            ),
            truncated: false,
            data: None,
            artifact: None,
            images: Vec::new(),
            cursor: None,
            status: haider_protocol::tool::ToolResultStatus::Completed,
            reason: None,
            presentation: None,
        },
        paths: vec![operation.path.clone()],
        post_digest: post_digest.clone(),
        checkpoint: CheckpointCapture {
            kind: checkpoint_kind,
            paths: vec![CheckpointCapturePath {
                path: relative_path_argument(relative)?.to_owned(),
                pre_digest: pre_bytes.as_deref().map(mutation_digest),
                pre_bytes,
                post_digest: Some(post_digest.clone()),
                truncated_reason: None,
            }],
            post_digest,
        },
    })
}

#[cfg(windows)]
// Keep the explicit Windows publication error branch so handle/drop ordering remains auditable.
#[allow(clippy::question_mark)]
fn apply_windows_edit(
    workspace_root: &OwnedFd,
    relative: &Path,
    operation: &FsEdit,
    expected_digest: Option<&str>,
) -> ToolResult<AppliedMutation> {
    if operation.edits.is_empty() {
        return Err(ToolError::invalid_argument("fs_edit edits cannot be empty"));
    }
    if operation.edits.iter().any(|edit| edit.old.is_empty()) {
        return Err(ToolError::invalid_argument(
            "fs_edit old anchors cannot be empty",
        ));
    }
    let (parent, target) =
        windows_mutation_target(workspace_root, relative, &operation.path, false)?;
    let mut source_file = open_windows_locked_file(&target, &operation.path)?;
    let source = windows_stable_snapshot(&mut source_file, &operation.path)?;
    let current_digest = mutation_digest(&source.bytes);
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
    let source_hash = blake3::hash(&source.bytes);
    let pre_bytes = source.bytes.clone();
    let mut edited =
        String::from_utf8(source.bytes).map_err(|error| ToolError::InvalidArgument {
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
    let permissions = source_file
        .metadata()
        .map_err(|error| ToolError::io("inspect target permissions", &operation.path, error))?
        .permissions();
    let temporary =
        stage_windows_content(parent.path(), &operation.path, bytes, Some(permissions))?;
    if let Err(error) = copy_windows_dacl(&source_file, &temporary.file, &operation.path) {
        let _ = delete_windows_entry(temporary.file, &operation.path);
        return Err(error);
    }
    let revalidated = match revalidate_windows_mutation(
        workspace_root,
        relative,
        &parent,
        &target,
        &operation.path,
        Some((&mut source_file, source.identity, source_hash)),
    ) {
        Ok(revalidated) => revalidated,
        Err(error) => {
            let _ = delete_windows_entry(temporary.file, &operation.path);
            return Err(error);
        }
    };
    if let Err(error) = publish_windows_temporary(
        temporary,
        &target,
        true,
        blake3::hash(bytes),
        &operation.path,
    ) {
        return Err(error);
    }
    drop(revalidated);
    drop(source_file);
    let post_digest = mutation_digest(bytes);
    Ok(AppliedMutation {
        result: BoundedResult {
            preview: format!(
                "edited {} ({} replacement{})",
                operation.path.display(),
                replacements,
                if replacements == 1 { "" } else { "s" }
            ),
            truncated: false,
            data: None,
            artifact: None,
            images: Vec::new(),
            cursor: None,
            status: haider_protocol::tool::ToolResultStatus::Completed,
            reason: None,
            presentation: None,
        },
        paths: vec![operation.path.clone()],
        post_digest: post_digest.clone(),
        checkpoint: CheckpointCapture {
            kind: CheckpointKind::Edit,
            paths: vec![CheckpointCapturePath {
                path: relative_path_argument(relative)?.to_owned(),
                pre_digest: Some(mutation_digest(&pre_bytes)),
                pre_bytes: Some(pre_bytes),
                post_digest: Some(post_digest.clone()),
                truncated_reason: None,
            }],
            post_digest,
        },
    })
}

#[cfg(windows)]
fn windows_mutation_target(
    workspace_root: &OwnedFd,
    relative: &Path,
    display_path: &Path,
    create_parents: bool,
) -> ToolResult<(OwnedFd, PathBuf)> {
    if relative.is_absolute() {
        return Err(ToolError::WorkspaceBoundary {
            workspace_root: workspace_root.path().to_path_buf(),
            requested_path: display_path.to_path_buf(),
            resolved_path: None,
        });
    }
    let leaf = relative
        .file_name()
        .ok_or_else(|| ToolError::invalid_argument("filesystem path has no leaf name"))?;
    let root = haider_platform::duplicate_workspace_directory(workspace_root)
        .map_err(|error| ToolError::io("duplicate workspace root", display_path, error))?;
    let relative_parent = relative.parent().unwrap_or(Path::new(""));
    let parent =
        haider_platform::open_workspace_subdirectory(root, relative_parent, create_parents)
            .map_err(|error| ToolError::io("open anchored mutation parent", display_path, error))?;
    let target = parent.path().join(leaf);
    Ok((parent, target))
}

#[cfg(windows)]
fn windows_is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(windows)]
fn require_windows_regular_file(
    target: &Path,
    display_path: &Path,
    metadata: &fs::Metadata,
) -> ToolResult<()> {
    if metadata.is_file() && !windows_is_reparse_point(metadata) {
        return Ok(());
    }
    Err(ToolError::PathChanged {
        path: display_path.to_path_buf(),
        message: format!("mutation target {} is not a real file", target.display()),
    })
}

#[cfg(windows)]
struct WindowsSnapshot {
    bytes: Vec<u8>,
    identity: haider_platform::WindowsFileIdentity,
}

#[cfg(windows)]
struct WindowsSource {
    _file: fs::File,
    identity: haider_platform::WindowsFileIdentity,
    hash: blake3::Hash,
    bytes: Vec<u8>,
}

#[cfg(windows)]
fn open_windows_current(target: &Path, display_path: &Path) -> ToolResult<fs::File> {
    use std::os::windows::fs::OpenOptionsExt as _;

    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let file = fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(target)
        .map_err(|error| ToolError::io("open mutation target", display_path, error))?;
    let metadata = file
        .metadata()
        .map_err(|error| ToolError::io("inspect mutation target", display_path, error))?;
    require_windows_regular_file(target, display_path, &metadata)?;
    Ok(file)
}

#[cfg(windows)]
fn open_windows_locked_file(target: &Path, display_path: &Path) -> ToolResult<fs::File> {
    let file = open_windows_current(target, display_path)?;
    file.lock()
        .map_err(|error| ToolError::io("lock mutation target", display_path, error))?;
    Ok(file)
}

#[cfg(windows)]
fn windows_stable_snapshot(
    file: &mut fs::File,
    display_path: &Path,
) -> ToolResult<WindowsSnapshot> {
    use std::os::windows::fs::MetadataExt as _;

    for _ in 0..SNAPSHOT_ATTEMPTS {
        let before = file
            .metadata()
            .map_err(|error| ToolError::io("inspect mutation snapshot", display_path, error))?;
        file.seek(SeekFrom::Start(0))
            .map_err(|error| ToolError::io("seek mutation snapshot", display_path, error))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|error| ToolError::io("read mutation snapshot", display_path, error))?;
        let after = file
            .metadata()
            .map_err(|error| ToolError::io("reinspect mutation snapshot", display_path, error))?;
        if before.file_attributes() == after.file_attributes()
            && before.creation_time() == after.creation_time()
            && before.last_write_time() == after.last_write_time()
            && before.file_size() == after.file_size()
            && before.file_size() == bytes.len() as u64
        {
            let identity = haider_platform::windows_file_identity(file).map_err(|error| {
                ToolError::io("identify mutation snapshot", display_path, error)
            })?;
            return Ok(WindowsSnapshot { bytes, identity });
        }
    }
    Err(ToolError::PathChanged {
        path: display_path.to_path_buf(),
        message: format!(
            "target content did not yield a stable snapshot after {SNAPSHOT_ATTEMPTS} attempts"
        ),
    })
}

#[cfg(windows)]
struct WindowsStagedFile {
    file: fs::File,
}

#[cfg(windows)]
fn stage_windows_content(
    parent: &Path,
    display_path: &Path,
    bytes: &[u8],
    permissions: Option<fs::Permissions>,
) -> ToolResult<WindowsStagedFile> {
    use std::os::windows::fs::OpenOptionsExt as _;

    const DELETE: u32 = 0x0001_0000;
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const WRITE_DAC: u32 = 0x0004_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const MAX_NAME_RETRIES: usize = 16;
    static NEXT_STAGING: AtomicU64 = AtomicU64::new(0);
    for _ in 0..MAX_NAME_RETRIES {
        let sequence = NEXT_STAGING.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".haider-write-{}-{sequence}.tmp",
            std::process::id()
        ));
        // `.write(true)` satisfies std's create/truncate validity check; the
        // explicit access_mode below still overrides the actual access bits.
        let mut file = match fs::OpenOptions::new()
            .write(true)
            .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE | WRITE_DAC)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(ToolError::io(
                    "create mutation temporary",
                    display_path,
                    error,
                ));
            }
        };
        let staged = file
            .write_all(bytes)
            .and_then(|()| {
                permissions
                    .clone()
                    .map_or(Ok(()), |permissions| file.set_permissions(permissions))
            })
            .and_then(|()| file.sync_all())
            .map_err(|error| ToolError::io("write mutation temporary", display_path, error));
        match staged {
            Ok(()) => return Ok(WindowsStagedFile { file }),
            Err(error) => {
                let _ = delete_windows_entry(file, display_path);
                return Err(error);
            }
        }
    }
    Err(ToolError::Runtime {
        message: format!(
            "could not allocate a unique mutation temporary for {}",
            display_path.display()
        ),
    })
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn copy_windows_dacl(
    source: &fs::File,
    destination: &fs::File,
    display_path: &Path,
) -> ToolResult<()> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Security::Authorization::{SE_FILE_OBJECT, SetSecurityInfo};
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, GetKernelObjectSecurity, GetSecurityDescriptorControl,
        GetSecurityDescriptorDacl, PROTECTED_DACL_SECURITY_INFORMATION, SE_DACL_PROTECTED,
        UNPROTECTED_DACL_SECURITY_INFORMATION,
    };

    const ERROR_INSUFFICIENT_BUFFER: i32 = 122;
    let mut needed = 0_u32;
    let measured = unsafe {
        GetKernelObjectSecurity(
            source.as_raw_handle(),
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            0,
            &raw mut needed,
        )
    };
    let measurement_error = std::io::Error::last_os_error();
    if measured == 0 && measurement_error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER) {
        return Err(ToolError::io(
            "measure target security descriptor",
            display_path,
            measurement_error,
        ));
    }
    let bytes = usize::try_from(needed)
        .map_err(|_| ToolError::invalid_argument("Windows security descriptor is too large"))?;
    let words = bytes.div_ceil(std::mem::size_of::<usize>());
    let mut descriptor = vec![0_usize; words];
    let descriptor_pointer = descriptor.as_mut_ptr().cast();
    let read = unsafe {
        GetKernelObjectSecurity(
            source.as_raw_handle(),
            DACL_SECURITY_INFORMATION,
            descriptor_pointer,
            needed,
            &raw mut needed,
        )
    };
    if read == 0 {
        return Err(ToolError::io(
            "read target security descriptor",
            display_path,
            std::io::Error::last_os_error(),
        ));
    }
    let mut control = 0_u16;
    let mut revision = 0_u32;
    let inspected = unsafe {
        GetSecurityDescriptorControl(descriptor_pointer, &raw mut control, &raw mut revision)
    };
    if inspected == 0 {
        return Err(ToolError::io(
            "inspect target DACL protection",
            display_path,
            std::io::Error::last_os_error(),
        ));
    }
    let protection = if control & SE_DACL_PROTECTED != 0 {
        PROTECTED_DACL_SECURITY_INFORMATION
    } else {
        UNPROTECTED_DACL_SECURITY_INFORMATION
    };
    let mut dacl_present = 0;
    let mut dacl_defaulted = 0;
    let mut dacl = std::ptr::null_mut();
    let extracted = unsafe {
        GetSecurityDescriptorDacl(
            descriptor_pointer,
            &raw mut dacl_present,
            &raw mut dacl,
            &raw mut dacl_defaulted,
        )
    };
    if extracted == 0 {
        return Err(ToolError::io(
            "extract target DACL",
            display_path,
            std::io::Error::last_os_error(),
        ));
    }
    if dacl_present == 0 {
        dacl = std::ptr::null_mut();
    }
    let written = unsafe {
        SetSecurityInfo(
            destination.as_raw_handle(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | protection,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            dacl,
            std::ptr::null_mut(),
        )
    };
    if written != 0 {
        Err(ToolError::io(
            "copy target DACL to staged file",
            display_path,
            std::io::Error::from_raw_os_error(written as i32),
        ))
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn revalidate_windows_mutation(
    workspace_root: &OwnedFd,
    relative: &Path,
    expected_parent: &OwnedFd,
    target: &Path,
    display_path: &Path,
    expected_source: Option<(
        &mut fs::File,
        haider_platform::WindowsFileIdentity,
        blake3::Hash,
    )>,
) -> ToolResult<Option<fs::File>> {
    let (parent, current_target) =
        windows_mutation_target(workspace_root, relative, display_path, false)?;
    let parent_identity = haider_platform::workspace_directory_identity(&parent)
        .map_err(|error| ToolError::io("identify mutation parent", display_path, error))?;
    let expected_parent_identity = haider_platform::workspace_directory_identity(expected_parent)
        .map_err(|error| {
        ToolError::io("identify original mutation parent", display_path, error)
    })?;
    if parent_identity != expected_parent_identity || current_target != target {
        return Err(ToolError::PathChanged {
            path: display_path.to_path_buf(),
            message: "mutation path changed before atomic replacement".into(),
        });
    }
    match expected_source {
        Some((locked_source, expected_identity, expected_hash)) => {
            let current = open_windows_current(target, display_path)?;
            let current_identity =
                haider_platform::windows_file_identity(&current).map_err(|error| {
                    ToolError::io("identify current mutation target", display_path, error)
                })?;
            // LockFileEx byte-range locks are mandatory on Windows. Reading
            // `current` here would conflict with our own exclusive lock even
            // though both handles belong to this process. Re-read through the
            // handle that owns the lock, and use the independently opened
            // handle only to prove that the anchored path still names it.
            let snapshot = windows_stable_snapshot(locked_source, display_path)?;
            if current_identity != expected_identity
                || snapshot.identity != expected_identity
                || blake3::hash(&snapshot.bytes) != expected_hash
            {
                return Err(ToolError::PathChanged {
                    path: display_path.to_path_buf(),
                    message: "target identity or content changed before atomic replacement".into(),
                });
            }
            Ok(Some(current))
        }
        None => match fs::symlink_metadata(target) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Ok(_) => Err(ToolError::PathChanged {
                path: display_path.to_path_buf(),
                message: "write target appeared before atomic creation".into(),
            }),
            Err(error) => Err(ToolError::io("reinspect write target", display_path, error)),
        },
    }
}

#[cfg(windows)]
fn publish_windows_temporary(
    mut temporary: WindowsStagedFile,
    target: &Path,
    replace_existing: bool,
    replacement_hash: blake3::Hash,
    display_path: &Path,
) -> ToolResult<()> {
    let snapshot = windows_stable_snapshot(&mut temporary.file, display_path)?;
    if blake3::hash(&snapshot.bytes) != replacement_hash {
        let _ = delete_windows_entry(temporary.file, display_path);
        return Err(ToolError::PathChanged {
            path: display_path.to_path_buf(),
            message: "staged file bytes differ from the approved content".into(),
        });
    }
    if let Err(error) = rename_windows_entry(&temporary.file, target, replace_existing) {
        let _ = delete_windows_entry(temporary.file, display_path);
        return Err(ToolError::io("publish staged file", display_path, error));
    }
    // Close the handle that now names the published file before reporting the
    // mutation complete. This keeps subsequent independent opens independent
    // of both validation and publication handle lifetimes.
    drop(temporary.file);
    Ok(())
}

#[cfg(windows)]
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
    let applied = match apply_windows_path(
        &workspace_dir,
        source_relative,
        destination_relative,
        operation,
    ) {
        Ok(applied) => applied,
        Err(error) => return MutationWorkerOutcome::ApplyFailed(error),
    };
    record_windows_mutation(applied, context)
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WindowsPathIdentity {
    file: haider_platform::WindowsFileIdentity,
    attributes: u32,
    creation_time: u64,
    last_write_time: u64,
    size: u64,
    directory: bool,
}

#[cfg(windows)]
struct WindowsPathEntry {
    identity: WindowsPathIdentity,
    handle: fs::File,
}

#[cfg(windows)]
fn open_windows_path_entry(
    path: &Path,
    display_path: &Path,
    delete_access: bool,
) -> ToolResult<WindowsPathEntry> {
    use std::os::windows::fs::OpenOptionsExt as _;

    const DELETE: u32 = 0x0001_0000;
    const GENERIC_READ: u32 = 0x8000_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let file = fs::OpenOptions::new()
        .access_mode(GENERIC_READ | if delete_access { DELETE } else { 0 })
        // Omitting FILE_SHARE_DELETE pins the exact namespace entry through
        // traversal. A checked directory cannot be renamed into a junction
        // before read_dir or a child open.
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|error| ToolError::io("open fs_path entry", display_path, error))?;
    windows_path_entry_from_file(file, display_path)
}

#[cfg(windows)]
fn windows_path_entry_from_file(
    file: fs::File,
    display_path: &Path,
) -> ToolResult<WindowsPathEntry> {
    use std::os::windows::fs::MetadataExt as _;

    let metadata = file
        .metadata()
        .map_err(|error| ToolError::io("inspect fs_path entry", display_path, error))?;
    if windows_is_reparse_point(&metadata) {
        return Err(ToolError::PathChanged {
            path: display_path.to_path_buf(),
            message: "fs_path refuses reparse points".into(),
        });
    }
    if !metadata.is_file() && !metadata.is_dir() {
        return Err(ToolError::invalid_argument(format!(
            "fs_path cannot mutate special path {}",
            display_path.display()
        )));
    }
    let identity = WindowsPathIdentity {
        file: haider_platform::windows_file_identity(&file)
            .map_err(|error| ToolError::io("identify fs_path entry", display_path, error))?,
        attributes: metadata.file_attributes(),
        creation_time: metadata.creation_time(),
        last_write_time: metadata.last_write_time(),
        size: metadata.file_size(),
        directory: metadata.is_dir(),
    };
    Ok(WindowsPathEntry {
        identity,
        handle: file,
    })
}

#[cfg(windows)]
fn open_windows_copy_directory(path: &Path, display_path: &Path) -> ToolResult<WindowsPathEntry> {
    use std::os::windows::fs::OpenOptionsExt as _;

    const DELETE: u32 = 0x0001_0000;
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let file = fs::OpenOptions::new()
        .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|error| ToolError::io("open copy destination directory", display_path, error))?;
    windows_path_entry_from_file(file, display_path)
}

#[cfg(windows)]
// Retain the strict Windows path-identity validation seam for dispatch paths that opt into it.
#[allow(dead_code)]
fn windows_path_identity(path: &Path, display_path: &Path) -> ToolResult<WindowsPathIdentity> {
    Ok(open_windows_path_entry(path, display_path, false)?.identity)
}

#[cfg(windows)]
// Retain the strict Windows path-identity validation seam while current dispatch validates handles directly.
#[allow(dead_code)]
fn require_windows_path_identity(
    path: &Path,
    display_path: &Path,
    expected: Option<WindowsPathIdentity>,
) -> ToolResult<()> {
    match expected {
        Some(expected) => {
            let current = windows_path_identity(path, display_path)?;
            if current == expected {
                Ok(())
            } else {
                Err(ToolError::PathChanged {
                    path: display_path.to_path_buf(),
                    message: "fs_path entry identity changed before mutation".into(),
                })
            }
        }
        None => match fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Ok(_) => Err(ToolError::PathChanged {
                path: display_path.to_path_buf(),
                message: "fs_path destination appeared before mutation".into(),
            }),
            Err(error) => Err(ToolError::io(
                "reinspect fs_path destination",
                display_path,
                error,
            )),
        },
    }
}

#[cfg(windows)]
// Keep the staged-copy error branch explicit so Windows staging ownership remains auditable.
#[allow(clippy::question_mark)]
fn apply_windows_path(
    workspace_dir: &OwnedFd,
    source_relative: &Path,
    destination_relative: Option<&Path>,
    operation: &FsPath,
) -> ToolResult<AppliedMutation> {
    if source_relative.as_os_str().is_empty() {
        return Err(ToolError::invalid_argument(
            "fs_path refuses to mutate the workspace root",
        ));
    }
    let (source_parent, source) =
        windows_mutation_target(workspace_dir, source_relative, &operation.source, false)?;
    let mut source_entry = open_windows_path_entry(
        &source,
        &operation.source,
        operation.operation != FsPathOperation::Copy,
    )?;
    let source_identity = source_entry.identity;
    let source_preimage =
        capture_windows_entry_preimage(&mut source_entry, source_relative, &operation.source)?;
    let source_post_digest = source_preimage.pre_digest.clone();
    let mut checkpoint_kind = CheckpointKind::Delete;
    let mut checkpoint_paths = vec![CheckpointCapturePath {
        post_digest: None,
        ..source_preimage.clone()
    }];
    let mut structural = Vec::new();
    structural.extend_from_slice(operation.operation_name().as_bytes());
    structural.push(0);
    structural.extend_from_slice(relative_path_argument(source_relative)?.as_bytes());

    let (result, paths) = match operation.operation {
        FsPathOperation::Delete => {
            let source_parent_check = haider_platform::workspace_directory_identity(&source_parent)
                .map_err(|error| {
                    ToolError::io("identify fs_path source parent", &operation.source, error)
                })?;
            let (fresh_parent, fresh_source) =
                windows_mutation_target(workspace_dir, source_relative, &operation.source, false)?;
            if haider_platform::workspace_directory_identity(&fresh_parent).map_err(|error| {
                ToolError::io("reidentify fs_path source parent", &operation.source, error)
            })? != source_parent_check
                || fresh_source != source
            {
                return Err(ToolError::PathChanged {
                    path: operation.source.clone(),
                    message: "fs_path source parent changed before delete".into(),
                });
            }
            remove_windows_entry_from_handle(&source, &operation.source, source_entry)?;
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
            structural.push(0);
            structural.extend_from_slice(relative_path_argument(destination_relative)?.as_bytes());
            let (destination_parent, destination_path) =
                windows_mutation_target(workspace_dir, destination_relative, destination, false)?;
            if source_identity.directory
                && haider_platform::workspace_directory_contains_identity(
                    &destination_parent,
                    source_identity.file,
                )
                .map_err(|error| {
                    ToolError::io("inspect fs_path destination ancestry", destination, error)
                })?
            {
                return Err(ToolError::invalid_argument(
                    "fs_path destination cannot be inside the source directory",
                ));
            }
            let mut destination_entry = match fs::symlink_metadata(&destination_path) {
                Ok(_) => Some(open_windows_path_entry(
                    &destination_path,
                    destination,
                    true,
                )?),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => {
                    return Err(ToolError::io(
                        "inspect fs_path destination",
                        destination,
                        error,
                    ));
                }
            };
            let destination_identity = destination_entry.as_ref().map(|entry| entry.identity);
            if destination_entry.is_some() && !operation.overwrite {
                return Err(ToolError::invalid_argument(format!(
                    "fs_path destination already exists: {}",
                    destination.display()
                )));
            }
            let mut destination_preimage = if let Some(entry) = destination_entry.as_mut() {
                capture_windows_entry_preimage(entry, destination_relative, destination)?
            } else {
                absent_checkpoint_path(destination_relative)?
            };
            if source_post_digest.is_none() && source_preimage.truncated_reason.is_some() {
                destination_preimage
                    .truncated_reason
                    .get_or_insert_with(|| {
                        "directory post-images are not representable by checkpoint_v1".into()
                    });
            }
            destination_preimage.post_digest = source_post_digest.clone();
            match operation.operation {
                FsPathOperation::Move => {
                    checkpoint_kind = CheckpointKind::Move;
                    checkpoint_paths.push(destination_preimage);
                }
                FsPathOperation::Copy => {
                    checkpoint_kind = if destination_entry.is_some() {
                        CheckpointKind::Write
                    } else {
                        CheckpointKind::Create
                    };
                    checkpoint_paths = vec![destination_preimage];
                }
                FsPathOperation::Delete => {
                    return Err(ToolError::Runtime {
                        message: "fs_path delete reached move/copy checkpoint planning".into(),
                    });
                }
            }
            if destination_identity.is_some_and(|identity| identity.file == source_identity.file) {
                return Err(ToolError::invalid_argument(
                    "fs_path source and destination identify the same path",
                ));
            }

            revalidate_windows_path_parents(
                workspace_dir,
                source_relative,
                &source_parent,
                &operation.source,
                destination_relative,
                &destination_parent,
                destination,
            )?;
            match operation.operation {
                FsPathOperation::Move => {
                    commit_windows_move(
                        source_entry.handle,
                        &destination_path,
                        &destination_parent,
                        destination_entry,
                        destination,
                    )?;
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
                    let staging = create_windows_path_staging(&destination_parent, destination)?;
                    let staged_entry = staging.path.join("entry");
                    let staged_entry_guard = match copy_windows_entry_from_handle(
                        &source,
                        &staged_entry,
                        &operation.source,
                        destination,
                        &mut structural,
                        source_entry,
                    ) {
                        Ok(entry) => entry,
                        Err(error) => {
                            cleanup_windows_path_staging(staging, destination);
                            return Err(error);
                        }
                    };
                    if let Err(error) = commit_windows_staged_entry(
                        staging,
                        &destination_path,
                        staged_entry_guard,
                        destination_entry,
                        destination,
                    ) {
                        return Err(error);
                    }
                    (
                        mutation_result(format!(
                            "copied {} to {}",
                            operation.source.display(),
                            destination.display()
                        )),
                        vec![destination.clone()],
                    )
                }
                FsPathOperation::Delete => {
                    return Err(ToolError::Runtime {
                        message: "fs_path delete reached move/copy dispatch".into(),
                    });
                }
            }
        }
    };
    let post_digest = mutation_digest(&structural);
    Ok(AppliedMutation {
        result,
        paths,
        post_digest: post_digest.clone(),
        checkpoint: CheckpointCapture {
            kind: checkpoint_kind,
            paths: checkpoint_paths,
            post_digest,
        },
    })
}

#[cfg(windows)]
fn revalidate_windows_path_parents(
    workspace_dir: &OwnedFd,
    source_relative: &Path,
    source_parent: &OwnedFd,
    source_display: &Path,
    destination_relative: &Path,
    destination_parent: &OwnedFd,
    destination_display: &Path,
) -> ToolResult<()> {
    let (fresh_source_parent, _) =
        windows_mutation_target(workspace_dir, source_relative, source_display, false)?;
    let (fresh_destination_parent, _) = windows_mutation_target(
        workspace_dir,
        destination_relative,
        destination_display,
        false,
    )?;
    let identity = |directory: &OwnedFd, display: &Path| {
        haider_platform::workspace_directory_identity(directory)
            .map_err(|error| ToolError::io("identify fs_path parent", display, error))
    };
    if identity(&fresh_source_parent, source_display)? != identity(source_parent, source_display)?
        || identity(&fresh_destination_parent, destination_display)?
            != identity(destination_parent, destination_display)?
    {
        return Err(ToolError::PathChanged {
            path: source_display.to_path_buf(),
            message: "fs_path source or destination parent changed before mutation".into(),
        });
    }
    Ok(())
}

#[cfg(windows)]
fn remove_windows_entry(path: &Path, display_path: &Path) -> ToolResult<()> {
    let entry = open_windows_path_entry(path, display_path, true)?;
    remove_windows_entry_from_handle(path, display_path, entry)
}

#[cfg(windows)]
fn remove_windows_entry_from_handle(
    path: &Path,
    display_path: &Path,
    entry: WindowsPathEntry,
) -> ToolResult<()> {
    let identity = entry.identity;
    if identity.directory {
        let mut entries = fs::read_dir(path)
            .map_err(|error| ToolError::io("list delete directory", display_path, error))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| ToolError::io("list delete directory", display_path, error))?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let child = entry.path();
            remove_windows_entry(&child, &display_path.join(entry.file_name()))?;
        }
    }
    delete_windows_entry(entry.handle, display_path)
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn delete_windows_entry(handle: fs::File, display_path: &Path) -> ToolResult<()> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_DISPOSITION_INFO, FileDispositionInfo, SetFileInformationByHandle,
    };

    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    let deleted = unsafe {
        SetFileInformationByHandle(
            handle.as_raw_handle(),
            FileDispositionInfo,
            (&raw const disposition).cast(),
            std::mem::size_of_val(&disposition) as u32,
        )
    };
    if deleted == 0 {
        Err(ToolError::io(
            "delete anchored filesystem entry",
            display_path,
            std::io::Error::last_os_error(),
        ))
    } else {
        Ok(())
    }
}

#[cfg(windows)]
struct WindowsPathStaging {
    path: PathBuf,
    _parent: OwnedFd,
    entry: WindowsPathEntry,
}

#[cfg(windows)]
fn create_windows_path_staging(
    parent: &OwnedFd,
    display_path: &Path,
) -> ToolResult<WindowsPathStaging> {
    const MAX_NAME_RETRIES: usize = 16;
    static NEXT_STAGING: AtomicU64 = AtomicU64::new(0);
    for _ in 0..MAX_NAME_RETRIES {
        let sequence = NEXT_STAGING.fetch_add(1, Ordering::Relaxed);
        let path = parent.path().join(format!(
            ".haider-path-{}-{sequence}.tmp",
            std::process::id()
        ));
        match fs::create_dir(&path) {
            Ok(()) => {
                let entry = match open_windows_path_entry(&path, display_path, true) {
                    Ok(entry) => entry,
                    Err(error) => {
                        let _ = fs::remove_dir(&path);
                        return Err(error);
                    }
                };
                let parent =
                    haider_platform::duplicate_workspace_directory(parent).map_err(|error| {
                        ToolError::io("retain path staging parent", display_path, error)
                    })?;
                return Ok(WindowsPathStaging {
                    path,
                    _parent: parent,
                    entry,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
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

#[cfg(windows)]
fn cleanup_windows_path_staging(staging: WindowsPathStaging, display_path: &Path) {
    let WindowsPathStaging {
        path,
        _parent,
        entry,
    } = staging;
    let _ = remove_windows_entry_from_handle(&path, display_path, entry);
}

#[cfg(windows)]
fn copy_windows_entry(
    source: &Path,
    destination: &Path,
    source_display: &Path,
    destination_display: &Path,
    structural: &mut Vec<u8>,
) -> ToolResult<WindowsPathEntry> {
    let source_entry = open_windows_path_entry(source, source_display, false)?;
    copy_windows_entry_from_handle(
        source,
        destination,
        source_display,
        destination_display,
        structural,
        source_entry,
    )
}

#[cfg(windows)]
// Keep explicit branch returns to preserve the directory/file fixture's parallel control flow.
#[allow(clippy::needless_return)]
fn copy_windows_entry_from_handle(
    source: &Path,
    destination: &Path,
    source_display: &Path,
    destination_display: &Path,
    structural: &mut Vec<u8>,
    mut source_entry: WindowsPathEntry,
) -> ToolResult<WindowsPathEntry> {
    let source_identity = source_entry.identity;
    if source_identity.directory {
        fs::create_dir(destination).map_err(|error| {
            ToolError::io(
                "create copy destination directory",
                destination_display,
                error,
            )
        })?;
        let destination_guard = open_windows_copy_directory(destination, destination_display)?;
        let mut entries = fs::read_dir(source)
            .map_err(|error| ToolError::io("list copy source", source_display, error))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| ToolError::io("list copy source", source_display, error))?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        structural.extend_from_slice(b"\0directory\0");
        structural.extend_from_slice(destination.as_os_str().as_encoded_bytes());
        for entry in entries {
            let name = entry.file_name();
            let _ = copy_windows_entry(
                &entry.path(),
                &destination.join(&name),
                &source_display.join(&name),
                &destination_display.join(&name),
                structural,
            )?;
        }
        destination_guard
            .handle
            .set_permissions(
                source_entry
                    .handle
                    .metadata()
                    .map_err(|error| {
                        ToolError::io("inspect copy source permissions", source_display, error)
                    })?
                    .permissions(),
            )
            .map_err(|error| {
                ToolError::io(
                    "set copy destination permissions",
                    destination_display,
                    error,
                )
            })?;
        let final_identity = haider_platform::windows_file_identity(&source_entry.handle)
            .map_err(|error| ToolError::io("reidentify copy source", source_display, error))?;
        if final_identity != source_identity.file {
            return Err(ToolError::PathChanged {
                path: source_display.to_path_buf(),
                message: "copy source identity changed while traversing".into(),
            });
        }
        return Ok(destination_guard);
    } else {
        use std::os::windows::fs::OpenOptionsExt as _;

        const DELETE: u32 = 0x0001_0000;
        const GENERIC_READ: u32 = 0x8000_0000;
        const GENERIC_WRITE: u32 = 0x4000_0000;
        const FILE_SHARE_READ: u32 = 0x0000_0001;
        let snapshot = windows_stable_snapshot(&mut source_entry.handle, source_display)?;
        if snapshot.identity != source_identity.file {
            return Err(ToolError::PathChanged {
                path: source_display.to_path_buf(),
                message: "copy source identity changed while opening".into(),
            });
        }
        // `.write(true)` satisfies std's create/truncate validity check; the
        // explicit access_mode below still overrides the actual access bits.
        let mut destination_file = fs::OpenOptions::new()
            .write(true)
            .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE)
            .share_mode(FILE_SHARE_READ)
            .create_new(true)
            .open(destination)
            .map_err(|error| {
                ToolError::io("create copy destination", destination_display, error)
            })?;
        destination_file
            .write_all(&snapshot.bytes)
            .and_then(|()| destination_file.sync_all())
            .map_err(|error| ToolError::io("write copy destination", destination_display, error))?;
        destination_file
            .set_permissions(
                source_entry
                    .handle
                    .metadata()
                    .map_err(|error| {
                        ToolError::io("inspect copy source permissions", source_display, error)
                    })?
                    .permissions(),
            )
            .map_err(|error| {
                ToolError::io(
                    "set copy destination permissions",
                    destination_display,
                    error,
                )
            })?;
        structural.extend_from_slice(b"\0file\0");
        structural.extend_from_slice(destination.as_os_str().as_encoded_bytes());
        structural.push(0);
        structural.extend_from_slice(&snapshot.bytes);
        let destination_guard =
            windows_path_entry_from_file(destination_file, destination_display)?;
        let final_identity = haider_platform::windows_file_identity(&source_entry.handle)
            .map_err(|error| ToolError::io("reidentify copy source", source_display, error))?;
        if final_identity != source_identity.file {
            return Err(ToolError::PathChanged {
                path: source_display.to_path_buf(),
                message: "copy source identity changed while traversing".into(),
            });
        }
        return Ok(destination_guard);
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn rename_windows_entry(
    handle: &fs::File,
    destination: &Path,
    replace_existing: bool,
) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_RENAME_INFO, FileRenameInfo, FileRenameInfoEx, SetFileInformationByHandle,
    };

    const FILE_RENAME_FLAG_REPLACE_IF_EXISTS: u32 = 0x0000_0001;
    const FILE_RENAME_FLAG_POSIX_SEMANTICS: u32 = 0x0000_0002;

    let mut destination = destination.as_os_str().encode_wide().collect::<Vec<_>>();
    if destination.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "rename destination is empty",
        ));
    }
    if destination.contains(&0) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "rename destination contains a NUL",
        ));
    }
    let name_bytes = destination
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .ok_or_else(|| std::io::Error::other("rename destination is too long"))?;
    destination.push(0);
    let buffer_bytes = std::mem::offset_of!(FILE_RENAME_INFO, FileName)
        .checked_add(name_bytes)
        .and_then(|bytes| bytes.checked_add(std::mem::size_of::<u16>()))
        .ok_or_else(|| std::io::Error::other("rename buffer size overflow"))?;
    let words = buffer_bytes.div_ceil(std::mem::size_of::<usize>());
    let mut buffer = vec![0_usize; words];
    let information = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    let information_class = if replace_existing {
        FileRenameInfoEx
    } else {
        FileRenameInfo
    };
    unsafe {
        if replace_existing {
            (*information).Anonymous.Flags =
                FILE_RENAME_FLAG_REPLACE_IF_EXISTS | FILE_RENAME_FLAG_POSIX_SEMANTICS;
        } else {
            (*information).Anonymous.ReplaceIfExists = false;
        }
        (*information).RootDirectory = std::ptr::null_mut();
        (*information).FileNameLength = u32::try_from(name_bytes)
            .map_err(|_| std::io::Error::other("rename destination is too long"))?;
        std::ptr::copy_nonoverlapping(
            destination.as_ptr(),
            std::ptr::addr_of_mut!((*information).FileName).cast::<u16>(),
            destination.len(),
        );
    }
    let renamed = unsafe {
        SetFileInformationByHandle(
            handle.as_raw_handle(),
            information_class,
            information.cast(),
            u32::try_from(buffer_bytes)
                .map_err(|_| std::io::Error::other("rename buffer is too large"))?,
        )
    };
    if renamed == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn commit_windows_move(
    source: fs::File,
    destination: &Path,
    destination_parent: &OwnedFd,
    destination_entry: Option<WindowsPathEntry>,
    destination_display: &Path,
) -> ToolResult<()> {
    if destination_entry.is_none() {
        return rename_windows_entry(&source, destination, false)
            .map_err(|error| ToolError::io("move anchored path", destination_display, error));
    }
    let staging = create_windows_path_staging(destination_parent, destination_display)?;
    let previous = staging.path.join("previous");
    let Some(destination_entry) = destination_entry else {
        return Err(ToolError::Runtime {
            message: "move destination identity disappeared before staging".into(),
        });
    };
    if let Err(error) = rename_windows_entry(&destination_entry.handle, &previous, false) {
        cleanup_windows_path_staging(staging, destination_display);
        return Err(ToolError::io(
            "stage previous move destination",
            destination_display,
            error,
        ));
    }
    if let Err(error) = rename_windows_entry(&source, destination, false) {
        let rollback = rename_windows_entry(&destination_entry.handle, destination, false);
        if rollback.is_ok() {
            cleanup_windows_path_staging(staging, destination_display);
            return Err(ToolError::io("move path", destination_display, error));
        }
        let rollback_error = rollback.err().ok_or_else(|| ToolError::Runtime {
            message: "failed move rollback reported neither success nor an error".into(),
        })?;
        return Err(ToolError::PathChanged {
            path: destination_display.to_path_buf(),
            message: format!(
                "move failed ({error}) and restoring the prior destination failed ({})",
                rollback_error
            ),
        });
    }
    let _ = remove_windows_entry_from_handle(&previous, destination_display, destination_entry);
    cleanup_windows_path_staging(staging, destination_display);
    Ok(())
}

#[cfg(windows)]
fn commit_windows_staged_entry(
    staging: WindowsPathStaging,
    destination: &Path,
    staged_entry: WindowsPathEntry,
    destination_entry: Option<WindowsPathEntry>,
    destination_display: &Path,
) -> ToolResult<()> {
    let previous = staging.path.join("previous");
    if let Some(destination_entry) = destination_entry.as_ref()
        && let Err(error) = rename_windows_entry(&destination_entry.handle, &previous, false)
    {
        cleanup_windows_path_staging(staging, destination_display);
        return Err(ToolError::io(
            "stage previous copy destination",
            destination_display,
            error,
        ));
    }
    if let Err(error) = rename_windows_entry(&staged_entry.handle, destination, false) {
        if let Some(destination_entry) = destination_entry.as_ref() {
            let rollback = rename_windows_entry(&destination_entry.handle, destination, false);
            if let Err(rollback_error) = rollback {
                return Err(ToolError::PathChanged {
                    path: destination_display.to_path_buf(),
                    message: format!(
                        "copy commit failed ({error}) and restoring the prior destination failed ({rollback_error})"
                    ),
                });
            }
        }
        cleanup_windows_path_staging(staging, destination_display);
        return Err(ToolError::io(
            "commit staged copy",
            destination_display,
            error,
        ));
    }
    if let Some(destination_entry) = destination_entry {
        let _ = remove_windows_entry_from_handle(&previous, destination_display, destination_entry);
    }
    cleanup_windows_path_staging(staging, destination_display);
    Ok(())
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
        checkpoint,
    } = applied;
    let effect = context.effect.clone();
    if let Err(error) = sync_mutation_parents(&paths) {
        return MutationWorkerOutcome::PostApplyFailed {
            error,
            effect,
            post_digest,
            checkpoint,
        };
    }
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
            checkpoint,
        },
        Err(error) => MutationWorkerOutcome::PostApplyFailed {
            error,
            effect,
            post_digest,
            checkpoint,
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
            Some((source, metadata, blake3::hash(&source_bytes), source_bytes))
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
        .map_or(0o644, |(_, metadata, _, _)| metadata.st_mode);
    if let Err(error) = write_patch_temporary(temporary_fd, mode, bytes, &operation.path) {
        remove_temporary(&parent, &temporary_name);
        return Err(error);
    }
    if let Err(error) = require_unchanged_target(
        &parent,
        &leaf,
        source.as_ref().map(|(_, metadata, _, _)| metadata),
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
    if let Some((source, _, source_hash, _)) = source.as_mut()
        && let Err(error) =
            require_unchanged_content(&parent, source, *source_hash, &operation.path)
    {
        remove_temporary(&parent, &temporary_name);
        return Err(error);
    }
    if let Err(error) = require_unchanged_target(
        &commit_parent,
        &leaf,
        source.as_ref().map(|(_, metadata, _, _)| metadata),
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
    let pre_bytes = source.take().map(|(_, _, _, bytes)| bytes);
    let checkpoint_kind = if pre_bytes.is_some() {
        CheckpointKind::Write
    } else {
        CheckpointKind::Create
    };
    Ok(AppliedMutation {
        result: BoundedResult {
            preview: format!(
                "wrote {} bytes to {}",
                bytes.len(),
                operation.path.display()
            ),
            truncated: false,
            data: None,
            artifact: None,
            images: Vec::new(),
            cursor: None,
            status: haider_protocol::tool::ToolResultStatus::Completed,
            reason: None,
            presentation: None,
        },
        paths: vec![operation.path.clone()],
        post_digest: post_digest.clone(),
        checkpoint: CheckpointCapture {
            kind: checkpoint_kind,
            paths: vec![CheckpointCapturePath {
                path: relative_path_argument(relative)?.to_owned(),
                pre_digest: pre_bytes.as_deref().map(mutation_digest),
                pre_bytes,
                post_digest: Some(post_digest.clone()),
                truncated_reason: None,
            }],
            post_digest,
        },
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
        checkpoint,
    } = applied;
    let effect = context.effect.clone();
    if let Err(error) = sync_mutation_parents(&paths) {
        return MutationWorkerOutcome::PostApplyFailed {
            error,
            effect,
            post_digest,
            checkpoint,
        };
    }
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
            checkpoint,
        },
        Err(error) => MutationWorkerOutcome::PostApplyFailed {
            error,
            effect,
            post_digest,
            checkpoint,
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
    let pre_bytes = source_bytes.clone();
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
            data: None,
            artifact: None,
            images: Vec::new(),
            cursor: None,
            status: haider_protocol::tool::ToolResultStatus::Completed,
            reason: None,
            presentation: None,
        },
        paths: vec![operation.path.clone()],
        post_digest: post_digest.clone(),
        checkpoint: CheckpointCapture {
            kind: CheckpointKind::Edit,
            paths: vec![CheckpointCapturePath {
                path: relative_path_argument(relative)?.to_owned(),
                pre_digest: Some(mutation_digest(&pre_bytes)),
                pre_bytes: Some(pre_bytes),
                post_digest: Some(post_digest.clone()),
                truncated_reason: None,
            }],
            post_digest,
        },
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
        checkpoint,
    } = applied;
    let effect = context.effect.clone();
    if let Err(error) = sync_mutation_parents(&paths) {
        return MutationWorkerOutcome::PostApplyFailed {
            error,
            effect,
            post_digest,
            checkpoint,
        };
    }
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
            checkpoint,
        },
        Err(error) => MutationWorkerOutcome::PostApplyFailed {
            error,
            effect,
            post_digest,
            checkpoint,
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
    let source_preimage = capture_unix_entry_preimage(
        &source_parent,
        &source_leaf,
        &source_metadata,
        source_relative,
        &operation.source,
    )?;
    let source_post_digest = source_preimage.pre_digest.clone();
    let mut checkpoint_kind = CheckpointKind::Delete;
    let mut checkpoint_paths = vec![CheckpointCapturePath {
        post_digest: None,
        ..source_preimage.clone()
    }];

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
            let mut destination_preimage = if let Some(metadata) = destination_metadata.as_ref() {
                capture_unix_entry_preimage(
                    &destination_parent,
                    &destination_leaf,
                    metadata,
                    destination_relative,
                    destination,
                )?
            } else {
                absent_checkpoint_path(destination_relative)?
            };
            if source_post_digest.is_none() && source_preimage.truncated_reason.is_some() {
                destination_preimage
                    .truncated_reason
                    .get_or_insert_with(|| {
                        "directory post-images are not representable by checkpoint_v1".into()
                    });
            }
            destination_preimage.post_digest = source_post_digest.clone();
            match operation.operation {
                FsPathOperation::Move => {
                    checkpoint_kind = CheckpointKind::Move;
                    checkpoint_paths.push(destination_preimage);
                }
                FsPathOperation::Copy => {
                    checkpoint_kind = if destination_metadata.is_some() {
                        CheckpointKind::Write
                    } else {
                        CheckpointKind::Create
                    };
                    checkpoint_paths = vec![destination_preimage];
                }
                FsPathOperation::Delete => {
                    return Err(ToolError::Runtime {
                        message: "fs_path delete reached move/copy checkpoint planning".into(),
                    });
                }
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
                FsPathOperation::Delete => {
                    return Err(ToolError::Runtime {
                        message: "fs_path delete reached move/copy dispatch".into(),
                    });
                }
            }
        }
    };

    let post_digest = mutation_digest(&structural);
    Ok(AppliedMutation {
        result,
        paths,
        post_digest: post_digest.clone(),
        checkpoint: CheckpointCapture {
            kind: checkpoint_kind,
            paths: checkpoint_paths,
            post_digest,
        },
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
        data: None,
        artifact: None,
        images: Vec::new(),
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

#[cfg(unix)]
enum StagedCopyCommitFailure {
    CleanupSafe(ToolError),
    PreserveStaging(ToolError),
}

#[cfg(unix)]
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
            rustix::fs::fsync(&destination_directory).map_err(|error| {
                anchored_io_error("sync staged copy directory", destination_path, error)
            })?;
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
#[cfg(unix)]
const MAX_SINGLE_READ_SNAPSHOT_BYTES: usize = i32::MAX as usize - 1;

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SnapshotBasis {
    CowClone,
    MetadataGuardedFallback,
}

#[cfg(unix)]
#[derive(Debug)]
struct FileSnapshot {
    bytes: Vec<u8>,
    basis: SnapshotBasis,
}

#[cfg(unix)]
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

#[cfg(all(unix, not(target_vendor = "apple")))]
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

#[cfg(all(unix, not(target_vendor = "apple")))]
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
        let created = match rustix::fs::mkdirat(&directory, &component, Mode::from_raw_mode(0o755))
        {
            Ok(()) => true,
            Err(rustix::io::Errno::EXIST) => false,
            Err(error) => {
                return Err(anchored_io_error(
                    "create write parent",
                    display_path,
                    error,
                ));
            }
        };
        if created {
            // Publish each new link in a multi-component parent chain before
            // descending. The final parent is synced after the file commit.
            rustix::fs::fsync(&directory).map_err(|error| {
                anchored_io_error("sync created write parent", display_path, error)
            })?;
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

#[cfg(unix)]
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

/// Redacts exactly once at the fs_read preview boundary. Raw bytes never enter
/// the preview, but a changed or oversized value is retained in CAS for the
/// existing owner-authorized item-inspection door.
async fn bounded_read<C>(
    contents: String,
    preview_contents: Option<String>,
    data: Option<ToolResultData>,
    sensitive_path: bool,
    bounds: ResultBounds,
    cas: &mut C,
) -> ToolResult<BoundedResult>
where
    C: CasSink,
{
    let semantic_truncated = matches!(
        &data,
        Some(ToolResultData::FsRead {
            truncated_reason: Some(_),
            ..
        })
    );
    let presented = if sensitive_path {
        "[REDACTED:sensitive_file]\n".to_owned()
    } else if matches!(&data, Some(ToolResultData::FsRead { .. })) {
        let entries = contents
            .lines()
            .map(|line| (line.to_owned(), line.ends_with('/')))
            .collect::<Vec<_>>();
        directory_preview(&entries).0
    } else {
        preview_contents.unwrap_or_else(|| contents.clone())
    };
    let redacted = crate::redact::redact_text(&presented);
    let presentation_reduced = presented != contents || redacted.replacements > 0;
    let truncated = semantic_truncated
        || presentation_reduced
        || contents.len() > bounds.max_preview_bytes
        || redacted.text.len() > bounds.max_preview_bytes;
    let artifact = Some(cas.put(contents.as_bytes()).await?);
    Ok(BoundedResult {
        preview: utf8_prefix(&redacted.text, bounds.max_preview_bytes).to_owned(),
        truncated,
        data,
        artifact,
        images: Vec::new(),
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
    let match_truncated = output.match_count > output.max_matches;
    let byte_truncated = output.total_bytes > bounds.max_preview_bytes;
    let preview_truncated = output.preview_saturated;
    let semantic_truncated = output.truncated_reason.is_some();
    let truncated = match_truncated || byte_truncated || preview_truncated || semantic_truncated;
    let truncated_reason = output
        .truncated_reason
        .or(if byte_truncated || preview_truncated {
            Some(ToolTruncationReason::ResultBytes)
        } else if match_truncated {
            Some(ToolTruncationReason::MatchLimit)
        } else {
            None
        });
    let artifact = Some(cas.put_file(output.complete.path()).await?);
    let mut result = BoundedResult {
        preview: output.preview,
        truncated,
        data: Some(ToolResultData::FsSearch {
            matches: output.structured,
            truncated_reason,
            binary_files_skipped: output.binary_files_skipped,
            skipped_sensitive: output.skipped_sensitive,
            files_scanned: output.files_scanned,
            bytes_scanned: output.bytes_scanned,
        }),
        artifact,
        images: Vec::new(),
        cursor: None,
        status: haider_protocol::tool::ToolResultStatus::Completed,
        reason: None,
        presentation: None,
    };
    enforce_search_wire_cap(&mut result)?;
    Ok(result)
}

fn enforce_search_wire_cap(result: &mut BoundedResult) -> ToolResult<()> {
    let encoded_len = |value: &BoundedResult| {
        serde_json::to_vec(value)
            .map(|encoded| encoded.len())
            .map_err(|error| ToolError::Runtime {
                message: format!("serialize bounded fs_search result: {error}"),
            })
    };
    while encoded_len(result)? > SEARCH_MAX_RESULT_BYTES {
        result.truncated = true;
        let Some(ToolResultData::FsSearch {
            matches,
            truncated_reason,
            ..
        }) = result.data.as_mut()
        else {
            return Err(ToolError::Runtime {
                message: "bounded fs_search result lost its structured payload".into(),
            });
        };
        *truncated_reason = Some(ToolTruncationReason::ResultBytes);
        if matches.pop().is_none() {
            if result.preview.is_empty() {
                return Err(ToolError::Runtime {
                    message: "fs_search metadata exceeded its hard result-byte cap".into(),
                });
            }
            result.preview.clear();
        }
    }
    Ok(())
}

async fn bounded_glob<C>(
    output: CappedOutput,
    bounds: ResultBounds,
    cas: &mut C,
) -> ToolResult<BoundedResult>
where
    C: CasSink,
{
    let byte_truncated = output.contents.len() > bounds.max_preview_bytes
        || output.preview.len() > bounds.max_preview_bytes;
    let truncated = output.truncated || byte_truncated;
    let truncated_reason = output
        .truncated_reason
        .or(byte_truncated.then_some(ToolTruncationReason::ResultBytes));
    let artifact = Some(cas.put(output.contents.as_bytes()).await?);
    Ok(BoundedResult {
        preview: utf8_prefix(&output.preview, bounds.max_preview_bytes).to_owned(),
        truncated,
        data: Some(ToolResultData::FsGlob {
            truncated_reason,
            skipped_sensitive: output.skipped_sensitive,
            files_scanned: output.files_scanned,
            collapsed_directories: output.collapsed_directories,
        }),
        artifact,
        images: Vec::new(),
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

#[cfg(unix)]
fn capture_unix_entry_preimage(
    parent: &OwnedFd,
    leaf: &OsStr,
    metadata: &rustix::fs::Stat,
    relative_path: &Path,
    display_path: &Path,
) -> ToolResult<CheckpointCapturePath> {
    if FileType::from_raw_mode(metadata.st_mode) == FileType::Directory {
        return Ok(CheckpointCapturePath {
            path: relative_path_argument(relative_path)?.to_owned(),
            pre_bytes: None,
            pre_digest: None,
            post_digest: None,
            truncated_reason: Some(
                "directory tree pre-images are not representable by checkpoint_v1".into(),
            ),
        });
    }
    if FileType::from_raw_mode(metadata.st_mode) != FileType::RegularFile {
        return Err(ToolError::PathChanged {
            path: display_path.to_path_buf(),
            message: "checkpoint capture refuses a non-file pre-image".into(),
        });
    }
    let source_fd = openat_nofollow(
        parent,
        leaf,
        OFlags::RDONLY,
        "open checkpoint pre-image",
        display_path,
    )?;
    let mut source = fs::File::from(source_fd);
    let opened = rustix::fs::fstat(&source)
        .map_err(|error| ToolError::io("identify checkpoint pre-image", display_path, error))?;
    if opened.st_dev != metadata.st_dev
        || opened.st_ino != metadata.st_ino
        || opened.st_size != metadata.st_size
    {
        return Err(ToolError::PathChanged {
            path: display_path.to_path_buf(),
            message: "checkpoint source identity or size changed before snapshotting".into(),
        });
    }
    let preimage_len = u64::try_from(opened.st_size).map_err(|_| ToolError::PathChanged {
        path: display_path.to_path_buf(),
        message: "checkpoint source reported a negative size".into(),
    })?;
    if preimage_len > haider_protocol::checkpoint::CHECKPOINT_PREIMAGE_MAX_BYTES {
        let mut hasher = blake3::Hasher::new();
        let mut total = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = source
                .read(&mut buffer)
                .map_err(|error| ToolError::io("hash checkpoint pre-image", display_path, error))?;
            if read == 0 {
                break;
            }
            total = total
                .checked_add(u64::try_from(read).map_err(|_| ToolError::PathChanged {
                    path: display_path.to_path_buf(),
                    message: "checkpoint read length does not fit u64".into(),
                })?)
                .ok_or_else(|| ToolError::PathChanged {
                    path: display_path.to_path_buf(),
                    message: "checkpoint pre-image size overflowed".into(),
                })?;
            hasher.update(&buffer[..read]);
        }
        let current =
            rustix::fs::statat(parent, leaf, AtFlags::SYMLINK_NOFOLLOW).map_err(|error| {
                anchored_io_error("recheck checkpoint pre-image", display_path, error)
            })?;
        let snapshot = rustix::fs::fstat(&source).map_err(|error| {
            ToolError::io("reidentify checkpoint pre-image", display_path, error)
        })?;
        if total != preimage_len
            || current.st_dev != metadata.st_dev
            || current.st_ino != metadata.st_ino
            || snapshot.st_dev != metadata.st_dev
            || snapshot.st_ino != metadata.st_ino
            || snapshot.st_size != metadata.st_size
        {
            return Err(ToolError::PathChanged {
                path: display_path.to_path_buf(),
                message: "checkpoint source changed while hashing an oversized pre-image".into(),
            });
        }
        return Ok(CheckpointCapturePath {
            path: relative_path_argument(relative_path)?.to_owned(),
            pre_bytes: None,
            pre_digest: Some(format!("blake3:{}", hasher.finalize().to_hex())),
            post_digest: None,
            truncated_reason: Some(format!(
                "pre-image is {preimage_len} bytes; checkpoint limit is {} bytes",
                haider_protocol::checkpoint::CHECKPOINT_PREIMAGE_MAX_BYTES
            )),
        });
    }
    let (bytes, _) = file_snapshot(parent, &mut source, display_path)?.parts();
    let current = rustix::fs::statat(parent, leaf, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|error| anchored_io_error("recheck checkpoint pre-image", display_path, error))?;
    let snapshot = rustix::fs::fstat(&source)
        .map_err(|error| ToolError::io("reidentify checkpoint pre-image", display_path, error))?;
    if current.st_dev != metadata.st_dev
        || current.st_ino != metadata.st_ino
        || snapshot.st_dev != metadata.st_dev
        || snapshot.st_ino != metadata.st_ino
    {
        return Err(ToolError::PathChanged {
            path: display_path.to_path_buf(),
            message: "checkpoint source identity changed while snapshotting".into(),
        });
    }
    Ok(CheckpointCapturePath {
        path: relative_path_argument(relative_path)?.to_owned(),
        pre_digest: Some(mutation_digest(&bytes)),
        pre_bytes: Some(bytes),
        post_digest: None,
        truncated_reason: None,
    })
}

#[cfg(windows)]
fn capture_windows_entry_preimage(
    entry: &mut WindowsPathEntry,
    relative_path: &Path,
    display_path: &Path,
) -> ToolResult<CheckpointCapturePath> {
    if entry.identity.directory {
        return Ok(CheckpointCapturePath {
            path: relative_path_argument(relative_path)?.to_owned(),
            pre_bytes: None,
            pre_digest: None,
            post_digest: None,
            truncated_reason: Some(
                "directory tree pre-images are not representable by checkpoint_v1".into(),
            ),
        });
    }
    if entry.identity.size > haider_protocol::checkpoint::CHECKPOINT_PREIMAGE_MAX_BYTES {
        let snapshot = windows_stable_digest(&mut entry.handle, display_path)?;
        if snapshot.identity != entry.identity {
            return Err(ToolError::PathChanged {
                path: display_path.to_path_buf(),
                message: "checkpoint source identity changed while hashing".into(),
            });
        }
        return Ok(CheckpointCapturePath {
            path: relative_path_argument(relative_path)?.to_owned(),
            pre_bytes: None,
            pre_digest: Some(snapshot.digest),
            post_digest: None,
            truncated_reason: Some(format!(
                "pre-image is {} bytes; checkpoint limit is {} bytes",
                entry.identity.size,
                haider_protocol::checkpoint::CHECKPOINT_PREIMAGE_MAX_BYTES
            )),
        });
    }
    let snapshot = windows_stable_snapshot(&mut entry.handle, display_path)?;
    if snapshot.identity != entry.identity.file {
        return Err(ToolError::PathChanged {
            path: display_path.to_path_buf(),
            message: "checkpoint source identity changed while snapshotting".into(),
        });
    }
    Ok(CheckpointCapturePath {
        path: relative_path_argument(relative_path)?.to_owned(),
        pre_digest: Some(mutation_digest(&snapshot.bytes)),
        pre_bytes: Some(snapshot.bytes),
        post_digest: None,
        truncated_reason: None,
    })
}

#[cfg(windows)]
struct WindowsStableDigest {
    digest: String,
    identity: WindowsPathIdentity,
}

#[cfg(windows)]
fn windows_stable_digest(
    file: &mut fs::File,
    display_path: &Path,
) -> ToolResult<WindowsStableDigest> {
    use std::os::windows::fs::MetadataExt as _;

    for _ in 0..SNAPSHOT_ATTEMPTS {
        let before = file
            .metadata()
            .map_err(|error| ToolError::io("inspect checkpoint digest", display_path, error))?;
        file.seek(SeekFrom::Start(0))
            .map_err(|error| ToolError::io("seek checkpoint digest", display_path, error))?;
        let mut hasher = blake3::Hasher::new();
        let mut total = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file
                .read(&mut buffer)
                .map_err(|error| ToolError::io("read checkpoint digest", display_path, error))?;
            if read == 0 {
                break;
            }
            total = total.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
            hasher.update(&buffer[..read]);
        }
        let after = file
            .metadata()
            .map_err(|error| ToolError::io("reinspect checkpoint digest", display_path, error))?;
        if before.file_attributes() == after.file_attributes()
            && before.creation_time() == after.creation_time()
            && before.last_write_time() == after.last_write_time()
            && before.file_size() == after.file_size()
            && before.file_size() == total
        {
            let file_identity = haider_platform::windows_file_identity(file).map_err(|error| {
                ToolError::io("identify checkpoint digest", display_path, error)
            })?;
            return Ok(WindowsStableDigest {
                digest: format!("blake3:{}", hasher.finalize().to_hex()),
                identity: WindowsPathIdentity {
                    file: file_identity,
                    attributes: after.file_attributes(),
                    creation_time: after.creation_time(),
                    last_write_time: after.last_write_time(),
                    size: after.file_size(),
                    directory: false,
                },
            });
        }
    }
    Err(ToolError::PathChanged {
        path: display_path.to_path_buf(),
        message: "checkpoint source changed while hashing its oversized pre-image".into(),
    })
}

fn absent_checkpoint_path(relative_path: &Path) -> ToolResult<CheckpointCapturePath> {
    Ok(CheckpointCapturePath {
        path: relative_path_argument(relative_path)?.to_owned(),
        pre_bytes: None,
        pre_digest: None,
        post_digest: None,
        truncated_reason: None,
    })
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
        FsSearchMode::Regex => "regex",
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

#[cfg(all(test, windows))]
#[allow(clippy::expect_used)]
mod windows_tests {
    use super::*;
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Storage::FileSystem::FILE_RENAME_INFO;

    #[test]
    // Keep the explicit remainder check because this fixture mirrors the Windows ABI alignment rule.
    #[allow(clippy::manual_is_multiple_of)]
    fn staged_publish_terminates_an_exactly_sized_verbatim_destination() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let parent = fs::canonicalize(directory.path()).expect("canonical temporary directory");
        assert!(
            parent.to_string_lossy().starts_with(r"\\?\"),
            "Windows canonical paths should exercise the verbatim-path rename route"
        );

        let target = (0_usize..)
            .map(|padding| parent.join(format!("windows-publish-{padding:x}.txt")))
            .find(|candidate| {
                let name_bytes =
                    candidate.as_os_str().encode_wide().count() * std::mem::size_of::<u16>();
                (std::mem::offset_of!(FILE_RENAME_INFO, FileName) + name_bytes)
                    % std::mem::size_of::<usize>()
                    == 0
            })
            .expect("find an exactly word-sized rename buffer");

        let created = b"created through the staged publish primitive";
        let temporary =
            stage_windows_content(&parent, &target, created, None).expect("stage created content");
        publish_windows_temporary(temporary, &target, false, blake3::hash(created), &target)
            .expect("publish created content");
        assert_eq!(fs::read(&target).expect("read created target"), created);

        let mut existing =
            open_windows_locked_file(&target, &target).expect("lock existing target");
        let replaced = b"replaced through the staged publish primitive";
        let temporary = stage_windows_content(&parent, &target, replaced, None)
            .expect("stage replacement content");
        publish_windows_temporary(temporary, &target, true, blake3::hash(replaced), &target)
            .expect("publish replacement content");
        assert_eq!(fs::read(&target).expect("read replaced target"), replaced);
        let mut old_bytes = Vec::new();
        existing
            .read_to_end(&mut old_bytes)
            .expect("read held old target");
        assert_eq!(old_bytes, created);
        drop(existing);

        let entries = fs::read_dir(&parent)
            .expect("list staging parent")
            .map(|entry| entry.expect("directory entry").file_name())
            .collect::<Vec<_>>();
        assert_eq!(
            entries,
            vec![target.file_name().expect("target name").to_owned()]
        );
    }
}
