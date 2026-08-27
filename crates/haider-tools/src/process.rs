//! Brokered process execution and live-process control.
//!
//! Processes are placed in their own process group. Output remains bytes from
//! pipe read through protocol `CommandOutput` delta (base64 is only the wire
//! encoding), and cancellation is supervised as TERM → grace → KILL. The
//! broker owns the supervisor finalizer, so every dispatched execution reaches
//! its one terminal claim even if the caller drops its wait future.
//!
//! Named containment residual: descendants that create a new session/process
//! group escape `killpg`. The supervisor never claims kernel-level containment
//! for those processes; stronger containment is reserved for a later wave.

use crate::broker::{EffectBroker, EffectOperation, FinalizerObserver, PermissionPolicy};
use crate::filesystem::CasSink;
use crate::shell::UserProcessExec;
use crate::{ToolError, ToolResult};
use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use haider_platform::WorkspaceDirectory as OwnedFd;
use haider_platform::{ProcessGroup, ProcessId as Pid, ProcessSignal as Signal};
use haider_protocol::effect::{EffectClass, EffectOutcome, WorkspaceMutation};
use haider_protocol::ids::{ArtifactRef, EffectId, WorkspaceRevision};
use haider_protocol::item::{ItemDelta, OutputStream, ToolStatus, TurnItem};
#[cfg(unix)]
use rustix::fs::{Mode, OFlags};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::env;
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio::time::{Sleep, sleep};

#[cfg(unix)]
type DirectoryIdentity = rustix::fs::Stat;
#[cfg(windows)]
type DirectoryIdentity = haider_platform::WindowsFileIdentity;

/// Maximum raw payload retained by one pipe-read chunk.
pub const PROCESS_OUTPUT_CHUNK_BYTES: usize = 8 * 1024;
/// Hard process-output ceiling. Keeping this equal to the adapter-input bound
/// guarantees every accepted process is reduced from its complete raw result.
pub const PROCESS_MAX_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
/// Bounded raw result retained solely as deterministic adapter input. The
/// process bound validator prevents diagnostic prefixes from being discarded.
pub const PROCESS_ADAPTER_INPUT_BYTES: usize = PROCESS_MAX_OUTPUT_BYTES;

/// Content-addresses the observable workspace tree before and after a process
/// execution. Directory ordering and all field boundaries are canonical;
/// atime is deliberately excluded so taking the snapshot cannot classify
/// itself as a mutation.
#[cfg(unix)]
pub fn workspace_state_digest(root: &Path) -> String {
    fn update_field(hasher: &mut blake3::Hasher, bytes: &[u8]) {
        hasher.update(&(bytes.len() as u64).to_be_bytes());
        hasher.update(bytes);
    }

    fn walk(root: &Path, relative: &Path, hasher: &mut blake3::Hasher) {
        let path = root.join(relative);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                hasher.update(b"metadata-error");
                update_field(hasher, relative.as_os_str().as_bytes());
                update_field(hasher, error.kind().to_string().as_bytes());
                return;
            }
        };
        update_field(hasher, relative.as_os_str().as_bytes());
        hasher.update(&metadata.mode().to_be_bytes());
        hasher.update(&metadata.uid().to_be_bytes());
        hasher.update(&metadata.gid().to_be_bytes());
        hasher.update(&metadata.ino().to_be_bytes());
        hasher.update(&metadata.mtime().to_be_bytes());
        hasher.update(&metadata.mtime_nsec().to_be_bytes());
        hasher.update(&metadata.ctime().to_be_bytes());
        hasher.update(&metadata.ctime_nsec().to_be_bytes());
        let file_type = metadata.file_type();
        if file_type.is_dir() {
            hasher.update(b"directory");
            let mut entries = match std::fs::read_dir(&path) {
                Ok(entries) => entries.filter_map(Result::ok).collect::<Vec<_>>(),
                Err(error) => {
                    hasher.update(b"read-dir-error");
                    update_field(hasher, error.kind().to_string().as_bytes());
                    return;
                }
            };
            entries.sort_by(|left, right| {
                left.file_name()
                    .as_bytes()
                    .cmp(right.file_name().as_bytes())
            });
            for entry in entries {
                walk(root, &relative.join(entry.file_name()), hasher);
            }
        } else if file_type.is_file() {
            hasher.update(b"file");
            hasher.update(&metadata.len().to_be_bytes());
            match std::fs::File::open(&path) {
                Ok(mut file) => {
                    let mut buffer = [0_u8; 64 * 1024];
                    loop {
                        match file.read(&mut buffer) {
                            Ok(0) => break,
                            Ok(read) => {
                                hasher.update(&buffer[..read]);
                            }
                            Err(error) => {
                                hasher.update(b"read-file-error");
                                update_field(hasher, error.kind().to_string().as_bytes());
                                break;
                            }
                        }
                    }
                }
                Err(error) => {
                    hasher.update(b"open-file-error");
                    update_field(hasher, error.kind().to_string().as_bytes());
                }
            }
        } else if file_type.is_symlink() {
            hasher.update(b"symlink");
            match std::fs::read_link(&path) {
                Ok(target) => update_field(hasher, target.as_os_str().as_bytes()),
                Err(error) => {
                    hasher.update(b"read-link-error");
                    update_field(hasher, error.kind().to_string().as_bytes());
                }
            }
        } else {
            hasher.update(b"special");
            hasher.update(&metadata.rdev().to_be_bytes());
        }
    }

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"haider.workspace-state.v1");
    walk(root, Path::new(""), &mut hasher);
    format!("blake3:{}", hasher.finalize().to_hex())
}

#[cfg(windows)]
pub fn workspace_state_digest(root: &Path) -> String {
    fn update_field(hasher: &mut blake3::Hasher, bytes: &[u8]) {
        hasher.update(&(bytes.len() as u64).to_be_bytes());
        hasher.update(bytes);
    }

    fn walk(root: &Path, relative: &Path, hasher: &mut blake3::Hasher) {
        let path = root.join(relative);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                hasher.update(b"metadata-error");
                update_field(hasher, relative.to_string_lossy().as_bytes());
                update_field(hasher, error.kind().to_string().as_bytes());
                return;
            }
        };
        update_field(hasher, relative.to_string_lossy().as_bytes());
        hasher.update(&metadata.len().to_be_bytes());
        let file_type = metadata.file_type();
        if file_type.is_dir() {
            hasher.update(b"directory");
            let mut entries = match std::fs::read_dir(&path) {
                Ok(entries) => entries.filter_map(Result::ok).collect::<Vec<_>>(),
                Err(error) => {
                    hasher.update(b"read-dir-error");
                    update_field(hasher, error.kind().to_string().as_bytes());
                    return;
                }
            };
            entries.sort_by_key(std::fs::DirEntry::file_name);
            for entry in entries {
                walk(root, &relative.join(entry.file_name()), hasher);
            }
        } else if file_type.is_file() {
            hasher.update(b"file");
            match std::fs::File::open(&path) {
                Ok(mut file) => {
                    let mut buffer = [0_u8; 64 * 1024];
                    loop {
                        match file.read(&mut buffer) {
                            Ok(0) => break,
                            Ok(read) => {
                                hasher.update(&buffer[..read]);
                            }
                            Err(error) => {
                                hasher.update(b"read-file-error");
                                update_field(hasher, error.kind().to_string().as_bytes());
                                break;
                            }
                        }
                    }
                }
                Err(error) => {
                    hasher.update(b"open-file-error");
                    update_field(hasher, error.kind().to_string().as_bytes());
                }
            }
        } else if file_type.is_symlink() {
            hasher.update(b"symlink");
            match std::fs::read_link(&path) {
                Ok(target) => update_field(hasher, target.to_string_lossy().as_bytes()),
                Err(error) => {
                    hasher.update(b"read-link-error");
                    update_field(hasher, error.kind().to_string().as_bytes());
                }
            }
        } else {
            hasher.update(b"special");
        }
    }

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"haider.workspace-state.v1");
    walk(root, Path::new(""), &mut hasher);
    format!("blake3:{}", hasher.finalize().to_hex())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessExec {
    pub call_id: String,
    pub command: String,
    pub cwd: PathBuf,
    pub env_allowlist: Vec<String>,
}

impl ProcessExec {
    pub fn new(call_id: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            call_id: call_id.into(),
            command: command.into(),
            cwd: PathBuf::from("."),
            env_allowlist: Vec::new(),
        }
    }

    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = cwd.into();
        self
    }

    pub fn with_env_allowlist(mut self, env_allowlist: Vec<String>) -> Self {
        self.env_allowlist = env_allowlist;
        self
    }

    pub fn started_item(&self) -> TurnItem {
        TurnItem::CommandExecution {
            call_id: self.call_id.clone(),
            command: self.command.clone(),
            status: ToolStatus::InProgress,
            exit_code: None,
        }
    }

    pub(crate) fn resolved(&self, workspace_root: &Path) -> ToolResult<Self> {
        if self.call_id.trim().is_empty() {
            return Err(ToolError::invalid_argument(
                "process_exec call_id must not be empty",
            ));
        }
        if self.command.trim().is_empty() {
            return Err(ToolError::invalid_argument(
                "process_exec command must not be empty",
            ));
        }
        let requested = if self.cwd.is_absolute() {
            self.cwd.clone()
        } else {
            workspace_root.join(&self.cwd)
        };
        let cwd = std::fs::canonicalize(&requested)
            .map_err(|error| ToolError::io("canonicalize process cwd", &requested, error))?;
        if !cwd.starts_with(workspace_root) {
            return Err(ToolError::WorkspaceBoundary {
                workspace_root: workspace_root.to_path_buf(),
                requested_path: self.cwd.clone(),
                resolved_path: Some(cwd),
            });
        }
        if !cwd.is_dir() {
            return Err(ToolError::invalid_argument(format!(
                "process cwd is not a directory: {}",
                cwd.display()
            )));
        }
        let mut env_allowlist = self.env_allowlist.clone();
        env_allowlist.sort();
        env_allowlist.dedup();
        if env_allowlist.iter().any(|name| name.is_empty()) {
            return Err(ToolError::invalid_argument(
                "process env_allowlist names must not be empty",
            ));
        }
        Ok(Self {
            call_id: self.call_id.clone(),
            command: self.command.clone(),
            cwd,
            env_allowlist,
        })
    }
}

impl EffectOperation for ProcessExec {
    fn effect_class(&self) -> EffectClass {
        EffectClass::ProcessExec
    }

    fn summary(&self) -> String {
        format!("run {}", self.command)
    }

    fn arguments(&self) -> ToolResult<Value> {
        process_arguments(&self.command, &self.cwd, &self.env_allowlist)
    }

    fn canonical_arguments(&self, workspace_root: &Path) -> ToolResult<Value> {
        let resolved = self.resolved(workspace_root)?;
        resolved.arguments()
    }

    fn approval_preview(&self) -> Vec<String> {
        vec![
            format!("Exact command: {}", self.command),
            format!(
                "Escaped command: {}",
                serde_json::to_string(&self.command)
                    .unwrap_or_else(|_| format!("{:?}", self.command))
            ),
            format!("Working directory: {}", self.cwd.display()),
        ]
    }
}

pub(crate) struct PreparedProcessExec {
    pub(crate) operation: ProcessExec,
    cwd_identity: DirectoryIdentity,
    bounds: ProcessBounds,
}

impl PreparedProcessExec {
    pub(crate) fn new(
        operation: &ProcessExec,
        workspace_root: &Path,
        workspace_dir: OwnedFd,
        bounds: ProcessBounds,
    ) -> ToolResult<Self> {
        if bounds.max_output_bytes == 0 || bounds.max_output_bytes > PROCESS_MAX_OUTPUT_BYTES {
            return Err(ToolError::invalid_argument(format!(
                "process max_output_bytes must be between 1 and {PROCESS_MAX_OUTPUT_BYTES}"
            )));
        }
        let operation = operation.resolved(workspace_root)?;
        let cwd_fd = open_directory_beneath(workspace_dir, workspace_root, &operation.cwd)?;
        let identity = directory_identity(&cwd_fd, &operation.cwd, "inspect process cwd")?;
        Ok(Self {
            operation,
            cwd_identity: identity,
            bounds,
        })
    }

    /// Re-walks the authorized cwd from a fresh workspace-root fd immediately
    /// before spawn and returns that newly confined handle.
    ///
    /// A path-following `stat` is insufficient: an attacker can move the
    /// authorized directory outside and replace its old name with a symlink to
    /// the same inode. `O_NOFOLLOW` on every fresh component rejects that
    /// same-inode relocation before the child can `fchdir`.
    pub(crate) fn cwd_for_spawn(
        &self,
        workspace_root: &Path,
        workspace_dir: OwnedFd,
    ) -> ToolResult<OwnedFd> {
        let cwd_fd = open_directory_beneath(workspace_dir, workspace_root, &self.operation.cwd)
            .map_err(|error| ToolError::PathChanged {
                path: self.operation.cwd.clone(),
                message: format!("cwd no longer resolves through the workspace root: {error}"),
            })?;
        let current = directory_identity(
            &cwd_fd,
            &self.operation.cwd,
            "verify authorized process cwd",
        )?;
        if !same_directory_identity(&current, &self.cwd_identity) {
            return Err(ToolError::PathChanged {
                path: self.operation.cwd.clone(),
                message: "process cwd identity changed before spawn".into(),
            });
        }
        Ok(cwd_fd)
    }
}

impl EffectOperation for PreparedProcessExec {
    fn effect_class(&self) -> EffectClass {
        self.operation.effect_class()
    }

    fn summary(&self) -> String {
        format!(
            "{} [timeout={}ms, output_cap={} bytes]",
            self.operation.summary(),
            self.bounds.wall_timeout.as_millis(),
            self.bounds.max_output_bytes,
        )
    }

    fn arguments(&self) -> ToolResult<Value> {
        self.operation.arguments()
    }

    fn canonical_arguments(&self, _workspace_root: &Path) -> ToolResult<Value> {
        self.operation.arguments()
    }

    fn approval_preview(&self) -> Vec<String> {
        let mut preview = self.operation.approval_preview();
        preview.push(format!(
            "Limits: {}ms wall clock, {} output bytes",
            self.bounds.wall_timeout.as_millis(),
            self.bounds.max_output_bytes,
        ));
        preview
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessBounds {
    pub max_inline_bytes: usize,
    /// Hard combined stdout/stderr byte cap. Reaching it terminates the whole
    /// process group and reports a typed failed process result.
    pub max_output_bytes: usize,
    /// Hard wall-clock limit from successful spawn through process exit.
    pub wall_timeout: Duration,
    pub kill_grace: Duration,
}

impl Default for ProcessBounds {
    fn default() -> Self {
        Self {
            max_inline_bytes: 8 * 1024,
            max_output_bytes: 1024 * 1024,
            wall_timeout: Duration::from_secs(60),
            kill_grace: Duration::from_secs(2),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessLimit {
    WallTimeout,
    OutputCap,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessOutputChunk {
    pub stream: OutputStream,
    pub chunk_b64: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessResult {
    pub call_id: String,
    pub effect: EffectId,
    pub command_arg_digest: String,
    pub workspace_revision: Option<WorkspaceRevision>,
    pub status: ToolStatus,
    pub exit_code: Option<i32>,
    /// Terminating signal when no conventional exit code exists.
    pub signal: Option<i32>,
    pub output_bytes: usize,
    /// BLAKE3 of the exact bounded stdout/stderr bytes accepted by the
    /// supervisor, in capture order.
    pub transcript_digest: String,
    pub inline_output: Vec<ProcessOutputChunk>,
    pub artifact: Option<ArtifactRef>,
    pub escalation_note: Option<String>,
    pub limit_reached: Option<ProcessLimit>,
    pub wall_timeout_ms: u64,
    pub max_output_bytes: usize,
    /// Largest raw transcript payload retained in memory at one time.
    ///
    /// This excludes the serialized spill file and includes the bounded
    /// adapter tail plus the chunk being serviced, making both caps observable
    /// in regression tests.
    pub transcript_high_water_bytes: usize,
    /// Supervisor ordering trace used by process-lifecycle regression tests.
    #[doc(hidden)]
    pub lifecycle_events: Vec<ProcessLifecycleEvent>,
}

impl ProcessResult {
    pub fn completed_item(&self, command: impl Into<String>) -> TurnItem {
        TurnItem::CommandExecution {
            call_id: self.call_id.clone(),
            command: command.into(),
            status: self.status,
            exit_code: self.exit_code,
        }
    }
}

/// Test-observable milestones around exit, sweeping, reaping, and registry
/// removal. They are deliberately facts, not control hooks.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessLifecycleEvent {
    LeaderExitObserved,
    GroupSweepStarted,
    GroupSweepCompleted,
    LeaderReaped,
    RegistryRemoved,
}

#[async_trait]
pub trait CommandOutputSink: Send + Sync {
    async fn emit(&self, call_id: &str, delta: ItemDelta) -> ToolResult<()>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NoopCommandOutputSink;

#[async_trait]
impl CommandOutputSink for NoopCommandOutputSink {
    async fn emit(&self, _call_id: &str, _delta: ItemDelta) -> ToolResult<()> {
        Ok(())
    }
}

pub struct ProcessExecution {
    call_id: String,
    effect: EffectId,
    cancel: watch::Sender<bool>,
    result: oneshot::Receiver<ToolResult<ProcessResult>>,
    finalizer: Option<FinalizerObserver>,
}

/// Cloneable cancellation capability for a live process execution.
///
/// It exposes only the existing supervised TERM → grace → KILL request; it
/// cannot wait for, inspect, or forge a process result.
#[derive(Clone)]
pub struct ProcessCancelHandle {
    cancel: watch::Sender<bool>,
}

impl ProcessCancelHandle {
    pub fn cancel(&self) {
        self.cancel.send_replace(true);
    }
}

impl std::fmt::Debug for ProcessExecution {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProcessExecution")
            .field("call_id", &self.call_id)
            .field("effect", &self.effect)
            .finish_non_exhaustive()
    }
}

impl ProcessExecution {
    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    pub fn effect(&self) -> &EffectId {
        &self.effect
    }

    pub fn cancel_handle(&self) -> ProcessCancelHandle {
        ProcessCancelHandle {
            cancel: self.cancel.clone(),
        }
    }

    /// Requests supervised TERM → grace → KILL cancellation.
    pub fn cancel(&self) {
        self.cancel.send_replace(true);
    }

    pub async fn wait(mut self) -> ToolResult<ProcessResult> {
        let result = (&mut self.result)
            .await
            .map_err(|error| ToolError::Runtime {
                message: format!("process supervisor stopped before reporting its result: {error}"),
            });
        if let Some(finalizer) = self.finalizer.take() {
            finalizer.observe();
        }
        result?
    }
}

impl Drop for ProcessExecution {
    fn drop(&mut self) {
        // A caller-side cancellation can drop the dispatcher future before it
        // gets a chance to explicitly cancel this handle. Dropping ownership
        // must therefore wake the broker-owned supervisor; the finalizer keeps
        // the TERM → grace → KILL and durable outcome work alive.
        self.cancel.send_replace(true);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessSignal {
    Hangup,
    Interrupt,
    Terminate,
    Kill,
    User1,
    User2,
}

impl ProcessSignal {
    fn rustix(self) -> Signal {
        match self {
            Self::Hangup => Signal::HUP,
            Self::Interrupt => Signal::INT,
            Self::Terminate => Signal::TERM,
            Self::Kill => Signal::KILL,
            Self::User1 => Signal::USR1,
            Self::User2 => Signal::USR2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessControlAction {
    SendSignal(ProcessSignal),
    StdinWrite(Vec<u8>),
    Kill,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessControl {
    pub call_id: String,
    pub action: ProcessControlAction,
}

impl ProcessControl {
    pub fn send_signal(call_id: impl Into<String>, signal: ProcessSignal) -> Self {
        Self {
            call_id: call_id.into(),
            action: ProcessControlAction::SendSignal(signal),
        }
    }

    pub fn stdin_write(call_id: impl Into<String>, bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            call_id: call_id.into(),
            action: ProcessControlAction::StdinWrite(bytes.into()),
        }
    }

    pub fn kill(call_id: impl Into<String>) -> Self {
        Self {
            call_id: call_id.into(),
            action: ProcessControlAction::Kill,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessControlResult {
    pub original_effect: EffectId,
    pub action: ProcessControlAction,
}

#[derive(Debug, Clone)]
struct ActiveProcess {
    effect: EffectId,
    pid: Pid,
    group: ProcessGroup,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    cancel: watch::Sender<bool>,
    live: Arc<AtomicBool>,
    leaked: Arc<AtomicBool>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ProcessRegistry {
    active: Arc<StdMutex<HashMap<String, ActiveProcess>>>,
}

impl ProcessRegistry {
    fn get(&self, call_id: &str) -> Option<ActiveProcess> {
        self.active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(call_id)
            .cloned()
    }

    fn contains(&self, call_id: &str) -> bool {
        self.active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(call_id)
    }

    fn insert(&self, call_id: String, process: ActiveProcess) -> ToolResult<()> {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active.contains_key(&call_id) {
            return Err(ToolError::invalid_argument(format!(
                "process call_id `{call_id}` is already live"
            )));
        }
        active.insert(call_id, process);
        Ok(())
    }

    fn remove(&self, call_id: &str, effect: &EffectId) {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active
            .get(call_id)
            .is_some_and(|process| &process.effect == effect)
        {
            active.remove(call_id);
        }
    }

    pub(crate) fn leaked_call_ids(&self) -> Vec<String> {
        let mut leaked = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|(_, process)| process.leaked.load(Ordering::Acquire))
            .map(|(call_id, _)| call_id.clone())
            .collect::<Vec<_>>();
        leaked.sort();
        leaked
    }

    pub(crate) fn cancel_all(&self) {
        let active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for process in active {
            process.cancel.send_replace(true);
        }
    }
}

#[derive(Debug, Clone)]
struct ProcessControlEffect {
    control: ProcessControl,
    original_effect: EffectId,
}

impl EffectOperation for ProcessControlEffect {
    fn effect_class(&self) -> EffectClass {
        EffectClass::ProcessExec
    }

    fn summary(&self) -> String {
        match self.control.action {
            ProcessControlAction::SendSignal(signal) => {
                format!("send {signal:?} to {}", self.control.call_id)
            }
            ProcessControlAction::StdinWrite(_) => {
                format!("write stdin to {}", self.control.call_id)
            }
            ProcessControlAction::Kill => format!("kill {}", self.control.call_id),
        }
    }

    fn arguments(&self) -> ToolResult<Value> {
        let action = match &self.control.action {
            ProcessControlAction::SendSignal(signal) => {
                json!({"kind": "send_signal", "signal": signal})
            }
            ProcessControlAction::StdinWrite(bytes) => {
                json!({"kind": "stdin_write", "bytes_b64": BASE64.encode(bytes)})
            }
            ProcessControlAction::Kill => json!({"kind": "kill"}),
        };
        Ok(json!({
            "action": action,
            "call_id": self.control.call_id,
            "original_effect": self.original_effect,
        }))
    }
}

impl EffectBroker {
    /// Starts a model-initiated command after broker authorization. The
    /// returned handle can be awaited or cancelled while the broker remains
    /// available for `process_control`.
    pub async fn process_exec<C, S>(
        &mut self,
        operation: &ProcessExec,
        policy: &PermissionPolicy,
        cas: C,
        output: S,
        bounds: ProcessBounds,
    ) -> ToolResult<ProcessExecution>
    where
        C: CasSink + 'static,
        S: CommandOutputSink + 'static,
    {
        self.start_process(operation, Some(policy), cas, output, bounds)
            .await
    }

    /// Starts a command typed directly by the user. It still receives all four
    /// journal phases, but the authorization phase records `user_typed`
    /// instead of consulting model-effect policy.
    pub async fn process_exec_user<C, S>(
        &mut self,
        operation: &UserProcessExec,
        cas: C,
        output: S,
        bounds: ProcessBounds,
    ) -> ToolResult<ProcessExecution>
    where
        C: CasSink + 'static,
        S: CommandOutputSink + 'static,
    {
        let _provenance = operation.provenance();
        self.start_process(operation.operation(), None, cas, output, bounds)
            .await
    }

    async fn start_process<C, S>(
        &mut self,
        operation: &ProcessExec,
        policy: Option<&PermissionPolicy>,
        cas: C,
        output: S,
        bounds: ProcessBounds,
    ) -> ToolResult<ProcessExecution>
    where
        C: CasSink + 'static,
        S: CommandOutputSink + 'static,
    {
        let prepared = PreparedProcessExec::new(
            operation,
            self.workspace_root(),
            self.duplicate_workspace_dir()?,
            bounds,
        )?;
        let operation = prepared.operation.clone();
        if self.processes.contains(&operation.call_id) {
            return Err(ToolError::invalid_argument(format!(
                "process call_id `{}` is already live",
                operation.call_id
            )));
        }
        let intent = match policy {
            Some(policy) => self.begin(&prepared, policy).await?,
            None => self.begin_user_typed(&prepared).await?,
        };
        let workspace_root = self.workspace_root().to_path_buf();
        let before_workspace_digest = tokio::task::spawn_blocking({
            let workspace_root = workspace_root.clone();
            move || workspace_state_digest(&workspace_root)
        })
        .await
        .map_err(|error| ToolError::Runtime {
            message: format!("workspace snapshot worker failed before process execution: {error}"),
        })?;
        let cwd_fd =
            match prepared.cwd_for_spawn(self.workspace_root(), self.duplicate_workspace_dir()?) {
                Ok(cwd_fd) => cwd_fd,
                Err(error) => return self.finish(&intent, Err(error)).await,
            };
        let mut command = shell_command(&operation.command);
        command
            .env_clear()
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        haider_platform::configure_process_environment(&mut command);
        for name in &operation.env_allowlist {
            if let Some(value) = env::var_os(name) {
                command.env(name, value);
            }
        }
        #[cfg(unix)]
        set_anchored_current_dir(&mut command, cwd_fd);
        #[cfg(windows)]
        set_anchored_current_dir(&mut command, &cwd_fd);
        haider_platform::configure_process_group(&mut command);
        #[cfg(windows)]
        let process_trace = windows_test_process_trace_enabled();
        #[cfg(windows)]
        if process_trace {
            let configured = command.as_std();
            eprintln!(
                "haider-daemon windows-process phase=spawn-begin program={} args_len={} cwd={}",
                configured.get_program().to_string_lossy(),
                configured.get_args().count(),
                operation.cwd.display(),
            );
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                #[cfg(windows)]
                if process_trace {
                    eprintln!("haider-daemon windows-process phase=spawn-failed error={error}");
                }
                return self
                    .finish(
                        &intent,
                        Err(ToolError::io("spawn process", &operation.cwd, error)),
                    )
                    .await;
            }
        };
        let Some(raw_pid) = child.id() else {
            return self
                .finish(
                    &intent,
                    Err(ToolError::Runtime {
                        message: "spawned process did not expose a process id".into(),
                    }),
                )
                .await;
        };
        #[cfg(windows)]
        // The in-process daemon test harness streams stderr under --nocapture;
        // keep this after successful OS spawn so absence distinguishes a
        // pre-spawn stall from an unobserved child.
        if process_trace {
            eprintln!("haider-daemon windows-process phase=daemon-spawned pid={raw_pid}");
        }
        let platform_group = match haider_platform::register_process_group(raw_pid) {
            Ok(group) => group,
            Err(error) => {
                let _ = child.start_kill();
                return self
                    .finish(
                        &intent,
                        Err(ToolError::Runtime {
                            message: format!(
                                "attach process {raw_pid} to its platform process group: {error}"
                            ),
                        }),
                    )
                    .await;
            }
        };
        let Some(pid) = i32::try_from(raw_pid).ok().and_then(Pid::from_raw) else {
            let _ = haider_platform::signal_process_group(
                platform_group,
                haider_platform::ProcessSignal::Kill,
            );
            haider_platform::release_process_group(platform_group);
            return self
                .finish(
                    &intent,
                    Err(ToolError::Runtime {
                        message: format!("spawned process id {raw_pid} is not representable"),
                    }),
                )
                .await;
        };
        let stdin = Arc::new(Mutex::new(child.stdin.take()));
        let stdout = child.stdout.take().ok_or_else(|| ToolError::Runtime {
            message: "spawned process stdout pipe is unavailable".into(),
        });
        let stderr = child.stderr.take().ok_or_else(|| ToolError::Runtime {
            message: "spawned process stderr pipe is unavailable".into(),
        });
        let (stdout, stderr) = match (stdout, stderr) {
            (Ok(stdout), Ok(stderr)) => (stdout, stderr),
            (Err(error), _) | (_, Err(error)) => {
                let _ = signal_group(pid, Signal::KILL);
                haider_platform::release_process_group(platform_group);
                return self.finish(&intent, Err(error)).await;
            }
        };
        let (cancel, cancel_receiver) = watch::channel(false);
        let live = Arc::new(AtomicBool::new(true));
        let leaked = Arc::new(AtomicBool::new(false));
        let active = ActiveProcess {
            effect: intent.effect.clone(),
            pid,
            group: platform_group,
            stdin: Arc::clone(&stdin),
            cancel: cancel.clone(),
            live: Arc::clone(&live),
            leaked: Arc::clone(&leaked),
        };
        if let Err(error) = self.processes.insert(operation.call_id.clone(), active) {
            let _ = signal_group(pid, Signal::KILL);
            haider_platform::release_process_group(platform_group);
            return self.finish(&intent, Err(error)).await;
        }

        let call_id = operation.call_id.clone();
        let effect = intent.effect.clone();
        let command_arg_digest = intent.args_digest.clone();
        let workspace_revision = intent.workspace_revision.clone();
        let registry = self.processes.clone();
        let finish = self.effect_finish(&intent);
        let (result_sender, result) = oneshot::channel();
        let finalizer_id = self.register_finalizer(async move {
            let mut process_result = supervise_process(Supervisor {
                child,
                stdout,
                stderr,
                stdin,
                pid,
                group: platform_group,
                call_id: call_id.clone(),
                effect: effect.clone(),
                command_arg_digest,
                workspace_revision,
                cancel: cancel_receiver,
                live,
                leaked,
                cas: Box::new(cas),
                output: Arc::new(output),
                bounds,
                #[cfg(windows)]
                process_trace,
            })
            .await;
            if !process_result.leaked {
                registry.remove(&call_id, &effect);
                if let Ok(result) = &mut process_result.result {
                    result
                        .lifecycle_events
                        .push(ProcessLifecycleEvent::RegistryRemoved);
                }
            }
            let after_workspace_digest =
                tokio::task::spawn_blocking(move || workspace_state_digest(&workspace_root))
                    .await
                    .map_err(|error| ToolError::Runtime {
                        message: format!(
                            "workspace snapshot worker failed after process execution: {error}"
                        ),
                    });
            let workspace_mutation = match after_workspace_digest {
                Ok(after) => (after != before_workspace_digest).then(|| WorkspaceMutation {
                    effect_id: effect.clone(),
                    mutation_digest: after,
                    workspace_revision: None,
                    subject_digest: None,
                }),
                Err(error) => {
                    process_result.result = Err(error);
                    None
                }
            };
            let outcome = match (&process_result.result, process_result.cancelled) {
                (_, true) => match &process_result.escalation_note {
                    Some(note) => EffectOutcome::CancelledEscalated { note: note.clone() },
                    None => EffectOutcome::Cancelled,
                },
                (Ok(result), false) => match result.limit_reached {
                    Some(limit) => EffectOutcome::Failed {
                        error: format!("process limit reached: {limit:?}"),
                    },
                    None => EffectOutcome::Ok,
                },
                (Err(error), false) => EffectOutcome::Failed {
                    error: error.to_string(),
                },
            };
            let terminal = finish
                .finish_outcome_with_workspace_mutation(outcome, workspace_mutation)
                .await;
            let delivered = match terminal {
                Ok(()) => process_result.result,
                Err(error) => Err(error),
            };
            let error = delivered.as_ref().err().cloned();
            let _ = result_sender.send(delivered);
            error
        });
        let finalizer = self.finalizer_observer(finalizer_id);
        Ok(ProcessExecution {
            call_id: operation.call_id,
            effect: intent.effect,
            cancel,
            result,
            finalizer: Some(finalizer),
        })
    }

    /// Applies a live-process mutation through its own brokered effect. The
    /// canonical arguments include the original process effect id.
    pub async fn process_control(
        &mut self,
        control: &ProcessControl,
        policy: &PermissionPolicy,
    ) -> ToolResult<ProcessControlResult> {
        let active = self.processes.get(&control.call_id).ok_or_else(|| {
            ToolError::invalid_argument(format!(
                "process call_id `{}` is not live",
                control.call_id
            ))
        })?;
        let operation = ProcessControlEffect {
            control: control.clone(),
            original_effect: active.effect.clone(),
        };
        let intent = self.begin(&operation, policy).await?;
        let result = apply_control(&active, control)
            .await
            .map(|()| ProcessControlResult {
                original_effect: active.effect,
                action: control.action.clone(),
            });
        self.finish(&intent, result).await
    }
}

struct Supervisor {
    child: tokio::process::Child,
    stdout: tokio::process::ChildStdout,
    stderr: tokio::process::ChildStderr,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    pid: Pid,
    group: ProcessGroup,
    call_id: String,
    effect: EffectId,
    command_arg_digest: String,
    workspace_revision: Option<WorkspaceRevision>,
    cancel: watch::Receiver<bool>,
    live: Arc<AtomicBool>,
    leaked: Arc<AtomicBool>,
    cas: Box<dyn CasSink>,
    output: Arc<dyn CommandOutputSink>,
    bounds: ProcessBounds,
    #[cfg(windows)]
    process_trace: bool,
}

struct SupervisorCompletion {
    result: ToolResult<ProcessResult>,
    cancelled: bool,
    leaked: bool,
    escalation_note: Option<String>,
}

#[derive(Debug)]
pub(crate) enum Captured {
    Chunk(OutputStream, Vec<u8>),
    ReadError(OutputStream, std::io::Error),
}

struct BufferedOutput {
    stream: OutputStream,
    bytes: Vec<u8>,
}

fn retain_adapter_tail(
    retained: &mut VecDeque<BufferedOutput>,
    retained_bytes: &mut usize,
    stream: OutputStream,
    bytes: &[u8],
) {
    let bytes = if bytes.len() > PROCESS_ADAPTER_INPUT_BYTES {
        bytes[bytes.len() - PROCESS_ADAPTER_INPUT_BYTES..].to_vec()
    } else {
        bytes.to_vec()
    };
    *retained_bytes = retained_bytes.saturating_add(bytes.len());
    retained.push_back(BufferedOutput { stream, bytes });
    while *retained_bytes > PROCESS_ADAPTER_INPUT_BYTES {
        let excess = retained_bytes.saturating_sub(PROCESS_ADAPTER_INPUT_BYTES);
        let Some(front) = retained.front_mut() else {
            *retained_bytes = 0;
            break;
        };
        if front.bytes.len() <= excess {
            *retained_bytes = retained_bytes.saturating_sub(front.bytes.len());
            retained.pop_front();
        } else {
            front.bytes.drain(..excess);
            *retained_bytes = retained_bytes.saturating_sub(excess);
        }
    }
}

struct TranscriptSpill {
    file: tempfile::NamedTempFile,
    first: bool,
}

impl TranscriptSpill {
    fn new() -> ToolResult<Self> {
        let mut file = tempfile::NamedTempFile::new()
            .map_err(|error| ToolError::io("create process transcript spill", "/tmp", error))?;
        file.write_all(b"[").map_err(|error| {
            ToolError::io("initialize process transcript spill", file.path(), error)
        })?;
        Ok(Self { file, first: true })
    }

    fn append(&mut self, stream: OutputStream, bytes: &[u8]) -> ToolResult<()> {
        if !self.first {
            self.file.write_all(b",").map_err(|error| {
                ToolError::io(
                    "append process transcript separator",
                    self.file.path(),
                    error,
                )
            })?;
        }
        self.first = false;
        let chunk = ProcessOutputChunk {
            stream,
            chunk_b64: BASE64.encode(bytes),
        };
        serde_json::to_writer(&mut self.file, &chunk).map_err(|error| ToolError::Runtime {
            message: format!("serialize process transcript chunk: {error}"),
        })
    }

    fn finish(mut self) -> ToolResult<tempfile::NamedTempFile> {
        self.file.write_all(b"]").map_err(|error| {
            ToolError::io("finish process transcript spill", self.file.path(), error)
        })?;
        self.file.flush().map_err(|error| {
            ToolError::io("flush process transcript spill", self.file.path(), error)
        })?;
        Ok(self.file)
    }
}

async fn supervise_process(supervisor: Supervisor) -> SupervisorCompletion {
    let Supervisor {
        mut child,
        stdout,
        stderr,
        stdin,
        pid,
        group,
        call_id,
        effect,
        command_arg_digest,
        workspace_revision,
        mut cancel,
        live,
        leaked,
        mut cas,
        output,
        bounds,
        #[cfg(windows)]
        process_trace,
    } = supervisor;
    let (captured_sender, mut captured) = mpsc::channel(1);
    tokio::spawn(read_output(
        stdout,
        OutputStream::Stdout,
        captured_sender.clone(),
    ));
    tokio::spawn(read_output(
        stderr,
        OutputStream::Stderr,
        captured_sender.clone(),
    ));
    drop(captured_sender);

    let mut transcript = Vec::<BufferedOutput>::new();
    let mut adapter_transcript = VecDeque::<BufferedOutput>::new();
    let mut adapter_payload_bytes = 0usize;
    let mut transcript_payload_bytes = 0usize;
    let mut transcript_high_water_bytes = 0usize;
    let mut spill = None;
    let mut transcript_failed = false;
    let mut output_bytes = 0usize;
    let mut transcript_hasher = blake3::Hasher::new();
    let mut fatal = None;
    let mut cancelled = *cancel.borrow();
    let mut cancel_open = true;
    let mut output_open = true;
    let mut exit_status = None;
    let mut leader_exit_observed = false;
    let mut leader_is_zombie = false;
    let mut leader_reaped = false;
    let mut kill_deadline: Option<Pin<Box<Sleep>>> = None;
    let mut pipe_drain_deadline: Option<Pin<Box<Sleep>>> = None;
    let mut escalation_notes = Vec::new();
    let mut lifecycle_events = Vec::new();
    let mut group_leaked = false;
    let mut limit_reached = None;
    let mut wall_deadline = Box::pin(sleep(bounds.wall_timeout));
    let mut wall_deadline_open = true;
    let mut exit_observation = Box::pin(observe_process_leader_exit(pid));
    #[cfg(windows)]
    let mut first_output_traced = false;

    if cancelled {
        begin_group_termination(
            group,
            pid,
            false,
            bounds.kill_grace,
            &mut kill_deadline,
            &mut fatal,
            &mut escalation_notes,
            &mut lifecycle_events,
        );
    }

    while !leader_reaped || output_open || kill_deadline.is_some() || pipe_drain_deadline.is_some()
    {
        tokio::select! {
            biased;
            changed = cancel.changed(), if !cancelled && cancel_open => {
                match changed {
                    Ok(()) if *cancel.borrow() => {
                        cancelled = true;
                        begin_group_termination(
                            group,
                            pid,
                            false,
                            bounds.kill_grace,
                            &mut kill_deadline,
                            &mut fatal,
                            &mut escalation_notes,
                            &mut lifecycle_events,
                        );
                    }
                    Ok(()) => {}
                    Err(_) => cancel_open = false,
                }
            }
            () = &mut wall_deadline, if wall_deadline_open && !leader_exit_observed && limit_reached.is_none() => {
                wall_deadline_open = false;
                limit_reached = Some(ProcessLimit::WallTimeout);
                begin_group_termination(
                    group,
                    pid,
                    false,
                    bounds.kill_grace,
                    &mut kill_deadline,
                    &mut fatal,
                    &mut escalation_notes,
                    &mut lifecycle_events,
                );
            }
            () = async {
                if let Some(deadline) = pipe_drain_deadline.as_mut() {
                    deadline.await;
                }
            }, if pipe_drain_deadline.is_some() => {
                pipe_drain_deadline = None;
                output_open = false;
                escalation_notes.push(
                    "output pipes remained open after the process-group sweep; \
                     a descendant may have escaped with setsid"
                        .into(),
                );
            }
            maybe_chunk = captured.recv(), if output_open => {
                match maybe_chunk {
                    Some(Captured::Chunk(stream, mut bytes)) => {
                        #[cfg(windows)]
                        if process_trace && !first_output_traced {
                            first_output_traced = true;
                            eprintln!(
                                "haider-daemon windows-process phase=first-output bytes={}",
                                bytes.len()
                            );
                        }
                        if cancelled || limit_reached.is_some() {
                            continue;
                        }
                        let remaining = bounds.max_output_bytes.saturating_sub(output_bytes);
                        let reached_cap = bytes.len() >= remaining;
                        bytes.truncate(remaining);
                        transcript_hasher.update(&bytes);
                        transcript_high_water_bytes = transcript_high_water_bytes.max(
                            transcript_payload_bytes
                                .saturating_add(adapter_payload_bytes)
                                .saturating_add(bytes.len())
                        );
                        output_bytes = output_bytes.saturating_add(bytes.len());
                        if !bytes.is_empty() {
                            retain_adapter_tail(
                                &mut adapter_transcript,
                                &mut adapter_payload_bytes,
                                stream,
                                &bytes,
                            );
                            let chunk_b64 = BASE64.encode(&bytes);
                            if let Err(error) = output
                                .emit(
                                    &call_id,
                                    ItemDelta::CommandOutput {
                                        stream,
                                        chunk_b64: chunk_b64.clone(),
                                    },
                                )
                                .await
                            {
                                fatal.get_or_insert(error);
                                begin_group_termination(
                                    group,
                                    pid,
                                    false,
                                    bounds.kill_grace,
                                    &mut kill_deadline,
                                    &mut fatal,
                                    &mut escalation_notes,
                                    &mut lifecycle_events,
                                );
                            }
                        }
                        if !transcript_failed
                            && spill.is_none()
                            && output_bytes > bounds.max_inline_bytes
                        {
                            match TranscriptSpill::new() {
                                Ok(mut created) => {
                                    transcript_payload_bytes = 0;
                                    for buffered in transcript.drain(..) {
                                        if let Err(error) =
                                            created.append(buffered.stream, &buffered.bytes)
                                        {
                                            fatal.get_or_insert(error);
                                            transcript_failed = true;
                                            break;
                                        }
                                    }
                                    if !transcript_failed {
                                        spill = Some(created);
                                    }
                                }
                                Err(error) => {
                                    fatal.get_or_insert(error);
                                    transcript.clear();
                                    transcript_payload_bytes = 0;
                                    transcript_failed = true;
                                }
                            }
                        }
                        if transcript_failed {
                            begin_group_termination(
                                group,
                                pid,
                                false,
                                bounds.kill_grace,
                                &mut kill_deadline,
                                &mut fatal,
                                &mut escalation_notes,
                                &mut lifecycle_events,
                            );
                            continue;
                        }
                        if let Some(active_spill) = spill.as_mut() {
                            if let Err(error) = active_spill.append(stream, &bytes) {
                                fatal.get_or_insert(error);
                                spill = None;
                                transcript_failed = true;
                                begin_group_termination(
                                    group,
                                    pid,
                                    false,
                                    bounds.kill_grace,
                                    &mut kill_deadline,
                                    &mut fatal,
                                    &mut escalation_notes,
                                    &mut lifecycle_events,
                                );
                            }
                        } else {
                            transcript_payload_bytes =
                                transcript_payload_bytes.saturating_add(bytes.len());
                            transcript.push(BufferedOutput { stream, bytes });
                        }
                        if reached_cap {
                            limit_reached = Some(ProcessLimit::OutputCap);
                            begin_group_termination(
                                group,
                                pid,
                                false,
                                bounds.kill_grace,
                                &mut kill_deadline,
                                &mut fatal,
                                &mut escalation_notes,
                                &mut lifecycle_events,
                            );
                        }
                    }
                    Some(Captured::ReadError(stream, error)) => {
                        fatal.get_or_insert_with(|| ToolError::Runtime {
                            message: format!("read {stream:?} from process `{call_id}`: {error}"),
                        });
                        begin_group_termination(
                            group,
                            pid,
                            false,
                            bounds.kill_grace,
                            &mut kill_deadline,
                            &mut fatal,
                            &mut escalation_notes,
                            &mut lifecycle_events,
                        );
                    }
                    None => {
                        output_open = false;
                        pipe_drain_deadline = None;
                    }
                }
            }
            observed = &mut exit_observation, if !leader_exit_observed => {
                leader_exit_observed = true;
                wall_deadline_open = false;
                leader_is_zombie = match observed {
                    Ok(()) => {
                        lifecycle_events.push(ProcessLifecycleEvent::LeaderExitObserved);
                        true
                    }
                    Err(error) => {
                        fatal.get_or_insert(error);
                        false
                    }
                };
                if kill_deadline.is_none() {
                    begin_group_termination(
                        group,
                        pid,
                        leader_is_zombie,
                        bounds.kill_grace,
                        &mut kill_deadline,
                        &mut fatal,
                        &mut escalation_notes,
                        &mut lifecycle_events,
                    );
                }
                if kill_deadline.is_none() {
                    reap_process_leader(
                        &mut child,
                        &stdin,
                        &call_id,
                        &mut exit_status,
                        &mut fatal,
                        &mut lifecycle_events,
                    )
                    .await;
                    #[cfg(windows)]
                    if process_trace && let Some(status) = exit_status.as_ref() {
                        eprintln!(
                            "haider-daemon windows-process phase=child-exited status={status}"
                        );
                    }
                    leader_reaped = true;
                    if output_open {
                        pipe_drain_deadline = Some(Box::pin(sleep(bounds.kill_grace)));
                    }
                }
            }
            () = async {
                if let Some(deadline) = kill_deadline.as_mut() {
                    deadline.await;
                }
            }, if kill_deadline.is_some() => {
                match platform_group_exists(group, pid) {
                    Ok(false) => {
                        kill_deadline = None;
                        lifecycle_events.push(ProcessLifecycleEvent::GroupSweepCompleted);
                    }
                    Ok(true) => {
                        if let Err(error) =
                            signal_platform_group_for_sweep(
                                group,
                                pid,
                                Signal::KILL,
                                leader_is_zombie,
                            )
                        {
                            let note = format!(
                                "SIGKILL escalation failed for process group {}: {error}",
                                pid.as_raw_nonzero()
                            );
                            escalation_notes.push(note);
                            fatal.get_or_insert(error);
                            // The deadline deliberately remains armed as the
                            // durable indication that escalation never
                            // completed. Return without awaiting the child.
                            group_leaked = true;
                            break;
                        }
                        kill_deadline = None;
                        lifecycle_events.push(ProcessLifecycleEvent::GroupSweepCompleted);
                    }
                    Err(error) => {
                        let note = format!(
                            "process-group probe failed during SIGKILL escalation: {error}"
                        );
                        escalation_notes.push(note);
                        fatal.get_or_insert(error);
                        group_leaked = true;
                        break;
                    }
                }
                if kill_deadline.is_none() && leader_exit_observed && !leader_reaped {
                    reap_process_leader(
                        &mut child,
                        &stdin,
                        &call_id,
                        &mut exit_status,
                        &mut fatal,
                        &mut lifecycle_events,
                    )
                    .await;
                    #[cfg(windows)]
                    if process_trace && let Some(status) = exit_status.as_ref() {
                        eprintln!(
                            "haider-daemon windows-process phase=child-exited status={status}"
                        );
                    }
                    leader_reaped = true;
                    if output_open {
                        pipe_drain_deadline = Some(Box::pin(sleep(bounds.kill_grace)));
                    }
                }
            }
        }
    }

    cancelled |= *cancel.borrow();
    if group_leaked {
        leaked.store(true, Ordering::Release);
    } else {
        live.store(false, Ordering::Release);
    }

    let artifact_result = if transcript_failed {
        Ok(None)
    } else {
        let file = if let Some(spill) = spill {
            spill.finish()
        } else {
            TranscriptSpill::new().and_then(|mut created| {
                for buffered in &transcript {
                    created.append(buffered.stream, &buffered.bytes)?;
                }
                created.finish()
            })
        };
        match file {
            Ok(file) => cas.put_file(file.path()).await.map(Some),
            Err(error) => Err(error),
        }
    };
    // Cancellation is sticky through artifact ingestion: a request arriving
    // while put_file is blocked owns both its success and failure arms.
    cancelled |= *cancel.borrow();
    let artifact = match artifact_result {
        Ok(artifact) => artifact,
        Err(error) => {
            fatal.get_or_insert(error);
            None
        }
    };
    let adapter_input = if output_bytes <= bounds.max_inline_bytes {
        transcript
    } else {
        adapter_transcript.into_iter().collect()
    };
    let inline_output = if transcript_failed {
        Vec::new()
    } else {
        adapter_input
            .into_iter()
            .map(|buffered| ProcessOutputChunk {
                stream: buffered.stream,
                chunk_b64: BASE64.encode(buffered.bytes),
            })
            .collect()
    };
    if exit_status.is_none() && !group_leaked {
        fatal.get_or_insert_with(|| ToolError::Runtime {
            message: format!("process `{call_id}` ended without an exit status"),
        });
    }
    let fatal_message = fatal.as_ref().map(ToString::to_string);
    if cancelled && let Some(message) = &fatal_message {
        escalation_notes.push(format!("supervisor error after cancellation: {message}"));
    }
    let escalation_note = (!escalation_notes.is_empty()).then(|| escalation_notes.join("; "));
    let process_result = ProcessResult {
        call_id,
        effect,
        command_arg_digest,
        workspace_revision,
        status: if cancelled {
            ToolStatus::Cancelled
        } else if limit_reached.is_some() {
            ToolStatus::Failed
        } else if exit_status
            .as_ref()
            .is_some_and(std::process::ExitStatus::success)
        {
            ToolStatus::Completed
        } else {
            ToolStatus::Failed
        },
        exit_code: exit_status
            .as_ref()
            .and_then(std::process::ExitStatus::code),
        signal: exit_status.as_ref().and_then(haider_platform::exit_signal),
        output_bytes,
        transcript_digest: format!("blake3:{}", transcript_hasher.finalize().to_hex()),
        inline_output,
        artifact,
        escalation_note: escalation_note.clone(),
        limit_reached,
        wall_timeout_ms: u64::try_from(bounds.wall_timeout.as_millis()).unwrap_or(u64::MAX),
        max_output_bytes: bounds.max_output_bytes,
        transcript_high_water_bytes,
        lifecycle_events,
    };
    let result = if cancelled {
        Ok(process_result)
    } else if let Some(error) = fatal {
        Err(error)
    } else {
        Ok(process_result)
    };
    SupervisorCompletion {
        result,
        cancelled,
        leaked: group_leaked,
        escalation_note,
    }
}

#[cfg(windows)]
fn windows_test_process_trace_enabled() -> bool {
    std::env::var("HAIDER_TEST_PROCESS_TRACE").is_ok_and(|value| value == "1")
}

pub(crate) async fn read_output<R>(
    mut reader: R,
    stream: OutputStream,
    sender: mpsc::Sender<Captured>,
) where
    R: AsyncRead + Unpin,
{
    let mut buffer = vec![0_u8; PROCESS_OUTPUT_CHUNK_BYTES];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => return,
            Ok(read) => {
                if sender
                    .send(Captured::Chunk(stream, buffer[..read].to_vec()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Err(error) => {
                let _ = sender.send(Captured::ReadError(stream, error)).await;
                return;
            }
        }
    }
}

async fn apply_control(active: &ActiveProcess, control: &ProcessControl) -> ToolResult<()> {
    if !active.live.load(Ordering::Acquire) {
        return Err(ToolError::invalid_argument(format!(
            "process call_id `{}` is no longer live",
            control.call_id
        )));
    }
    match &control.action {
        ProcessControlAction::SendSignal(signal) => {
            signal_platform_group(active.group, active.pid, signal.rustix())
        }
        ProcessControlAction::StdinWrite(bytes) => {
            let mut stdin = active.stdin.lock().await;
            let pipe = stdin.as_mut().ok_or_else(|| {
                ToolError::invalid_argument(format!(
                    "process call_id `{}` has no writable stdin",
                    control.call_id
                ))
            })?;
            pipe.write_all(bytes)
                .await
                .map_err(|error| ToolError::Runtime {
                    message: format!("write stdin to process `{}`: {error}", control.call_id),
                })?;
            pipe.flush().await.map_err(|error| ToolError::Runtime {
                message: format!("flush stdin for process `{}`: {error}", control.call_id),
            })
        }
        ProcessControlAction::Kill => {
            active.cancel.send_replace(true);
            Ok(())
        }
    }
}

pub(crate) fn signal_group(pid: Pid, signal: Signal) -> ToolResult<()> {
    let group =
        haider_platform::process_group(Some(pid.id())).ok_or_else(|| ToolError::Runtime {
            message: format!(
                "process group {} is no longer registered",
                pid.as_raw_nonzero()
            ),
        })?;
    signal_platform_group(group, pid, signal)
}

fn signal_platform_group(group: ProcessGroup, pid: Pid, signal: Signal) -> ToolResult<()> {
    haider_platform::signal_process_group(group, signal).map_err(|error| ToolError::Runtime {
        message: format!(
            "send signal {signal:?} to process group {}: {error}",
            pid.as_raw_nonzero()
        ),
    })
}

/// Signals the original group during a supervisor sweep. `ESRCH` after the
/// zombie leader has pinned the PGID means no live member remains to signal,
/// so the sweep is complete rather than failed.
pub(crate) fn signal_group_for_sweep(
    pid: Pid,
    signal: Signal,
    leader_is_zombie: bool,
) -> ToolResult<bool> {
    let Some(group) = haider_platform::process_group(Some(pid.id())) else {
        #[cfg(windows)]
        return Ok(false);
        #[cfg(unix)]
        return Err(ToolError::Runtime {
            message: "signal process group: invalid process id".into(),
        });
    };
    signal_platform_group_for_sweep(group, pid, signal, leader_is_zombie)
}

pub(crate) fn signal_platform_group_for_sweep(
    group: ProcessGroup,
    pid: Pid,
    signal: Signal,
    leader_is_zombie: bool,
) -> ToolResult<bool> {
    match haider_platform::signal_process_group(group, signal) {
        Ok(()) => {
            // A Windows KILL terminates the entire Job Object. Drop the exact
            // token immediately so a terminal escalation cannot retain a
            // stale job handle (or later act on a recycled PID).
            #[cfg(windows)]
            if signal == Signal::KILL {
                haider_platform::release_process_group(group);
            }
            Ok(true)
        }
        Err(error) if haider_platform::process_error_is_missing(&error) => Ok(false),
        // Darwin reports EPERM when a group contains only the caller's zombie
        // child. Since that zombie pins this exact PGID, no live member of the
        // original group remains signalable in this case.
        Err(error) if leader_is_zombie && haider_platform::process_error_is_permission(&error) => {
            Ok(false)
        }
        Err(error) => Err(ToolError::Runtime {
            message: format!(
                "send signal {signal:?} to process group {}: {error}",
                pid.as_raw_nonzero()
            ),
        }),
    }
}

pub(crate) fn process_group_exists(pid: Pid) -> ToolResult<bool> {
    let Some(group) = haider_platform::process_group(Some(pid.id())) else {
        // Windows Job handles are process-owned authorities. If a daemon
        // restarts, KILL_ON_JOB_CLOSE has already swept every old task and a
        // missing token therefore means dead. Never reconstruct authority
        // from the recyclable numeric PID.
        #[cfg(windows)]
        return Ok(false);
        #[cfg(unix)]
        return Err(ToolError::Runtime {
            message: "probe process group: invalid process id".into(),
        });
    };
    platform_group_exists(group, pid)
}

pub(crate) fn platform_group_exists(group: ProcessGroup, pid: Pid) -> ToolResult<bool> {
    haider_platform::process_group_exists(group).map_err(|error| ToolError::Runtime {
        message: format!("probe process group {}: {error}", pid.as_raw_nonzero()),
    })
}

pub(crate) async fn observe_process_leader_exit(pid: Pid) -> ToolResult<()> {
    loop {
        let exited = match haider_platform::process_leader_exited(pid) {
            Ok(exited) => exited,
            Err(error) => {
                return Err(ToolError::Runtime {
                    message: format!(
                        "observe process leader {} exit without reaping: {error}",
                        pid.as_raw_nonzero()
                    ),
                });
            }
        };
        if exited {
            return Ok(());
        }
        sleep(Duration::from_millis(1)).await;
    }
}

pub(crate) async fn reap_process_leader(
    child: &mut Child,
    stdin: &Arc<Mutex<Option<ChildStdin>>>,
    call_id: &str,
    exit_status: &mut Option<std::process::ExitStatus>,
    fatal: &mut Option<ToolError>,
    lifecycle_events: &mut Vec<ProcessLifecycleEvent>,
) {
    match child.wait().await {
        Ok(status) => *exit_status = Some(status),
        Err(error) => {
            fatal.get_or_insert_with(|| ToolError::Runtime {
                message: format!("wait for process `{call_id}`: {error}"),
            });
        }
    }
    lifecycle_events.push(ProcessLifecycleEvent::LeaderReaped);
    *stdin.lock().await = None;
}

/// Starts or completes a process-group sweep.
///
/// Ordering invariant for the post-exit path: the leader exit must first be
/// observed with non-reaping `waitid(..., WNOWAIT)`, and the caller must keep
/// that zombie unreaped until this sweep reaches `GroupSweepCompleted`. The
/// zombie keeps the PGID allocated, so every probe/signal below is guaranteed
/// to target the original group rather than a recycled one.
#[allow(clippy::too_many_arguments)]
pub(crate) fn begin_group_termination(
    group: ProcessGroup,
    pid: Pid,
    leader_is_zombie: bool,
    grace: Duration,
    deadline: &mut Option<Pin<Box<Sleep>>>,
    fatal: &mut Option<ToolError>,
    notes: &mut Vec<String>,
    lifecycle_events: &mut Vec<ProcessLifecycleEvent>,
) {
    if deadline.is_some() {
        return;
    }
    lifecycle_events.push(ProcessLifecycleEvent::GroupSweepStarted);
    match platform_group_exists(group, pid) {
        Ok(false) => lifecycle_events.push(ProcessLifecycleEvent::GroupSweepCompleted),
        Ok(true) => {
            match signal_platform_group_for_sweep(group, pid, Signal::TERM, leader_is_zombie) {
                Ok(true) => *deadline = Some(Box::pin(sleep(grace))),
                Ok(false) => {
                    lifecycle_events.push(ProcessLifecycleEvent::GroupSweepCompleted);
                }
                Err(error) => {
                    notes.push(format!(
                        "SIGTERM failed for process group {}: {error}",
                        pid.as_raw_nonzero()
                    ));
                    fatal.get_or_insert(error);
                    *deadline = Some(Box::pin(sleep(grace)));
                }
            }
        }
        Err(error) => {
            notes.push(format!(
                "process-group probe failed before SIGTERM: {error}"
            ));
            fatal.get_or_insert(error);
            *deadline = Some(Box::pin(sleep(grace)));
        }
    }
}

#[cfg(unix)]
fn open_directory_beneath(
    mut directory: OwnedFd,
    workspace_root: &Path,
    cwd: &Path,
) -> ToolResult<OwnedFd> {
    let relative = cwd
        .strip_prefix(workspace_root)
        .map_err(|_| ToolError::WorkspaceBoundary {
            workspace_root: workspace_root.to_path_buf(),
            requested_path: cwd.to_path_buf(),
            resolved_path: Some(cwd.to_path_buf()),
        })?;
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            continue;
        };
        directory = rustix::fs::openat(
            &directory,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| ToolError::io("open anchored process cwd", cwd, error))?;
    }
    Ok(directory)
}

#[cfg(windows)]
fn open_directory_beneath(
    directory: OwnedFd,
    workspace_root: &Path,
    cwd: &Path,
) -> ToolResult<OwnedFd> {
    let relative = cwd
        .strip_prefix(workspace_root)
        .map_err(|_| ToolError::WorkspaceBoundary {
            workspace_root: workspace_root.to_path_buf(),
            requested_path: cwd.to_path_buf(),
            resolved_path: Some(cwd.to_path_buf()),
        })?;
    let anchored = haider_platform::open_workspace_subdirectory(directory, relative, false)
        .map_err(|error| ToolError::io("open anchored process cwd", cwd, error))?;
    if anchored.path() != cwd {
        return Err(ToolError::PathChanged {
            path: cwd.to_path_buf(),
            message: "process cwd identity changed before spawn".into(),
        });
    }
    Ok(anchored)
}

#[cfg(unix)]
#[allow(unsafe_code)]
pub(crate) fn set_anchored_current_dir(command: &mut Command, cwd_fd: OwnedFd) {
    use std::os::unix::process::CommandExt as _;
    // SAFETY: `pre_exec` runs after fork and before exec. This closure performs
    // only the async-signal-safe `fchdir` syscall and owns the directory fd,
    // so the child cannot follow the authorized pathname again.
    unsafe {
        command
            .as_std_mut()
            .pre_exec(move || rustix::process::fchdir(&cwd_fd).map_err(std::io::Error::from));
    }
}

#[cfg(windows)]
pub(crate) fn set_anchored_current_dir(command: &mut Command, cwd: &OwnedFd) {
    command.current_dir(cwd.path());
}

#[cfg(unix)]
fn directory_identity(
    directory: &OwnedFd,
    path: &Path,
    operation: &'static str,
) -> ToolResult<DirectoryIdentity> {
    rustix::fs::fstat(directory).map_err(|error| ToolError::io(operation, path, error))
}

#[cfg(windows)]
fn directory_identity(
    directory: &OwnedFd,
    path: &Path,
    operation: &'static str,
) -> ToolResult<DirectoryIdentity> {
    haider_platform::workspace_directory_identity(directory)
        .map_err(|error| ToolError::io(operation, path, error))
}

#[cfg(unix)]
fn same_directory_identity(left: &DirectoryIdentity, right: &DirectoryIdentity) -> bool {
    left.st_dev == right.st_dev && left.st_ino == right.st_ino
}

#[cfg(windows)]
fn same_directory_identity(left: &DirectoryIdentity, right: &DirectoryIdentity) -> bool {
    left == right
}

#[cfg(unix)]
pub(crate) fn shell_command(script: &str) -> Command {
    let executable = unix_shell_executable(env::var_os("SHELL"), is_executable_file);
    let mut command = Command::new(executable);
    command.arg("-c").arg(script);
    command
}

#[cfg(unix)]
fn unix_shell_executable(
    configured_shell: Option<std::ffi::OsString>,
    is_executable: impl Fn(&Path) -> bool,
) -> std::ffi::OsString {
    for shell in [Path::new("/bin/zsh"), Path::new("/bin/sh")] {
        if is_executable(shell) {
            return shell.as_os_str().to_owned();
        }
    }
    if let Some(shell) = configured_shell
        && trustworthy_posix_shell(Path::new(&shell), &is_executable)
    {
        return shell;
    }
    std::ffi::OsString::from("sh")
}

#[cfg(unix)]
fn trustworthy_posix_shell(shell: &Path, is_executable: &impl Fn(&Path) -> bool) -> bool {
    shell.is_absolute()
        && matches!(
            shell.file_name().and_then(|name| name.to_str()),
            Some("sh" | "ash" | "bash" | "dash" | "ksh" | "mksh")
        )
        && is_executable(shell)
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(windows)]
pub(crate) fn shell_command(script: &str) -> Command {
    let mut command = Command::new(haider_platform::windows_powershell());
    command.args([
        "-NoLogo",
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-Command",
        script,
    ]);
    command
}

pub(crate) fn process_arguments(
    command: &str,
    cwd: &Path,
    env_allowlist: &[String],
) -> ToolResult<Value> {
    let cwd = cwd.to_str().ok_or_else(|| ToolError::InvalidArgument {
        message: format!("process cwd is not valid UTF-8: {}", cwd.display()),
    })?;
    let mut env_allowlist = env_allowlist.to_vec();
    env_allowlist.sort();
    env_allowlist.dedup();
    Ok(json!({
        "command": command,
        "cwd": cwd,
        "env_allowlist": env_allowlist,
    }))
}

#[cfg(test)]
mod shell_command_tests {
    use super::shell_command;

    #[cfg(unix)]
    #[test]
    fn unix_shell_command_prefers_zsh_with_sh_fallback_and_preserves_script_bytes() {
        let script = "printf ' exact bytes é\n'";
        let command = shell_command(script);
        let command = command.as_std();
        let expected =
            super::unix_shell_executable(std::env::var_os("SHELL"), super::is_executable_file);
        assert_eq!(command.get_program(), expected);
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [std::ffi::OsStr::new("-c"), std::ffi::OsStr::new(script)]
        );
        assert_eq!(
            super::unix_shell_executable(None, |path| path == std::path::Path::new("/bin/zsh")),
            "/bin/zsh"
        );
        assert_eq!(
            super::unix_shell_executable(None, |path| path == std::path::Path::new("/bin/sh")),
            "/bin/sh"
        );
    }

    /// Android/Termux has neither `/bin/zsh` nor `/bin/sh`; prefer a
    /// trustworthy executable `$SHELL`, then let PATH resolve `sh`.
    ///
    /// MUTATION CHECK: replace the configured-shell return with `"sh"`.
    /// Expected runtime failure: the first assertion receives `"sh"` instead
    /// of the executable Termux-style `$SHELL` path.
    #[cfg(unix)]
    #[test]
    fn unix_shell_resolution_falls_back_to_path_sh_when_bin_shells_are_absent() {
        let configured = std::path::PathBuf::from("/data/data/com.termux/files/usr/bin/bash");
        let selected =
            super::unix_shell_executable(Some(configured.clone().into_os_string()), |path| {
                path == configured
            });
        assert_eq!(selected, configured.as_os_str());

        let path_default = super::unix_shell_executable(None, |_| false);
        assert_eq!(path_default, "sh");

        let incompatible = std::path::PathBuf::from("/data/data/com.termux/files/usr/bin/fish");
        let rejected =
            super::unix_shell_executable(Some(incompatible.clone().into_os_string()), |path| {
                path == incompatible
            });
        assert_eq!(rejected, "sh");
    }

    #[cfg(windows)]
    #[test]
    fn windows_shell_command_is_absolute_system32_powershell_and_preserves_script_bytes() {
        let script = "[Console]::Out.Write(' exact bytes ')";
        let command = shell_command(script);
        let command = command.as_std();
        let program = std::path::Path::new(command.get_program());
        assert!(program.is_absolute());
        assert!(
            program
                .to_string_lossy()
                .to_ascii_lowercase()
                .ends_with(r"\system32\windowspowershell\v1.0\powershell.exe")
        );
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                std::ffi::OsStr::new("-NoLogo"),
                std::ffi::OsStr::new("-NoProfile"),
                std::ffi::OsStr::new("-NonInteractive"),
                std::ffi::OsStr::new("-ExecutionPolicy"),
                std::ffi::OsStr::new("Bypass"),
                std::ffi::OsStr::new("-Command"),
                std::ffi::OsStr::new(script),
            ]
        );
    }
}
