//! Brokered process execution and live-process control.
//!
//! Processes are placed in their own process group. Output remains bytes from
//! pipe read through protocol `CommandOutput` delta (base64 is only the wire
//! encoding), and cancellation is supervised as TERM → grace → KILL. The
//! broker owns the supervisor finalizer, so every dispatched execution reaches
//! its one terminal claim even if the caller drops its wait future.

use crate::broker::{EffectBroker, EffectOperation, FinalizerObserver, PermissionPolicy};
use crate::filesystem::CasSink;
use crate::{ToolError, ToolResult};
use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use haider_protocol::effect::{EffectClass, EffectOutcome};
use haider_protocol::ids::{ArtifactRef, EffectId};
use haider_protocol::item::{ItemDelta, OutputStream, ToolStatus, TurnItem};
use rustix::process::{Pid, Signal, kill_process_group, test_kill_process_group};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::env;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio::time::{Sleep, sleep};

const OUTPUT_CHUNK_BYTES: usize = 8 * 1024;

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

    fn resolved(&self, workspace_root: &Path) -> ToolResult<Self> {
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessBounds {
    pub max_inline_bytes: usize,
    pub kill_grace: Duration,
}

impl Default for ProcessBounds {
    fn default() -> Self {
        Self {
            max_inline_bytes: 8 * 1024,
            kill_grace: Duration::from_secs(2),
        }
    }
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
    pub status: ToolStatus,
    pub exit_code: Option<i32>,
    pub output_bytes: usize,
    pub inline_output: Vec<ProcessOutputChunk>,
    pub artifact: Option<ArtifactRef>,
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
    finalizer: FinalizerObserver,
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

    /// Requests supervised TERM → grace → KILL cancellation.
    pub fn cancel(&self) {
        self.cancel.send_replace(true);
    }

    pub async fn wait(self) -> ToolResult<ProcessResult> {
        let result = self.result.await.map_err(|error| ToolError::Runtime {
            message: format!("process supervisor stopped before reporting its result: {error}"),
        });
        self.finalizer.observe();
        result?
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
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    cancel: watch::Sender<bool>,
    live: Arc<AtomicBool>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ProcessRegistry {
    active: Arc<Mutex<HashMap<String, ActiveProcess>>>,
}

impl ProcessRegistry {
    async fn get(&self, call_id: &str) -> Option<ActiveProcess> {
        self.active.lock().await.get(call_id).cloned()
    }

    async fn contains(&self, call_id: &str) -> bool {
        self.active.lock().await.contains_key(call_id)
    }

    async fn insert(&self, call_id: String, process: ActiveProcess) -> ToolResult<()> {
        let mut active = self.active.lock().await;
        if active.contains_key(&call_id) {
            return Err(ToolError::invalid_argument(format!(
                "process call_id `{call_id}` is already live"
            )));
        }
        active.insert(call_id, process);
        Ok(())
    }

    async fn remove(&self, call_id: &str, effect: &EffectId) {
        let mut active = self.active.lock().await;
        if active
            .get(call_id)
            .is_some_and(|process| &process.effect == effect)
        {
            active.remove(call_id);
        }
    }

    pub(crate) async fn cancel_all(&self) {
        let active = self
            .active
            .lock()
            .await
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
        operation: &ProcessExec,
        cas: C,
        output: S,
        bounds: ProcessBounds,
    ) -> ToolResult<ProcessExecution>
    where
        C: CasSink + 'static,
        S: CommandOutputSink + 'static,
    {
        self.start_process(operation, None, cas, output, bounds)
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
        let operation = operation.resolved(self.workspace_root())?;
        if self.processes.contains(&operation.call_id).await {
            return Err(ToolError::invalid_argument(format!(
                "process call_id `{}` is already live",
                operation.call_id
            )));
        }
        let intent = match policy {
            Some(policy) => self.begin(&operation, policy).await?,
            None => self.begin_user_typed(&operation).await?,
        };
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(&operation.command)
            .current_dir(&operation.cwd)
            .env_clear()
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        for name in &operation.env_allowlist {
            if let Some(value) = env::var_os(name) {
                command.env(name, value);
            }
        }
        command.as_std_mut().process_group(0);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
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
        let Some(pid) = i32::try_from(raw_pid).ok().and_then(Pid::from_raw) else {
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
                return self.finish(&intent, Err(error)).await;
            }
        };
        let (cancel, cancel_receiver) = watch::channel(false);
        let live = Arc::new(AtomicBool::new(true));
        let active = ActiveProcess {
            effect: intent.effect.clone(),
            pid,
            stdin: Arc::clone(&stdin),
            cancel: cancel.clone(),
            live: Arc::clone(&live),
        };
        if let Err(error) = self
            .processes
            .insert(operation.call_id.clone(), active)
            .await
        {
            let _ = signal_group(pid, Signal::KILL);
            return self.finish(&intent, Err(error)).await;
        }

        let call_id = operation.call_id.clone();
        let effect = intent.effect.clone();
        let registry = self.processes.clone();
        let finish = self.effect_finish(&intent);
        let (result_sender, result) = oneshot::channel();
        let finalizer_id = self.register_finalizer(async move {
            let process_result = supervise_process(Supervisor {
                child,
                stdout,
                stderr,
                stdin,
                pid,
                call_id: call_id.clone(),
                effect: effect.clone(),
                cancel: cancel_receiver,
                live,
                cas: Box::new(cas),
                output: Arc::new(output),
                bounds,
            })
            .await;
            registry.remove(&call_id, &effect).await;
            let outcome = match &process_result {
                Ok(result) if result.status == ToolStatus::Cancelled => EffectOutcome::Cancelled,
                Ok(_) => EffectOutcome::Ok,
                Err(error) => EffectOutcome::Failed {
                    error: error.to_string(),
                },
            };
            let terminal = finish.finish_outcome(outcome).await;
            let delivered = match terminal {
                Ok(()) => process_result,
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
            finalizer,
        })
    }

    /// Applies a live-process mutation through its own brokered effect. The
    /// canonical arguments include the original process effect id.
    pub async fn process_control(
        &mut self,
        control: &ProcessControl,
        policy: &PermissionPolicy,
    ) -> ToolResult<ProcessControlResult> {
        let active = self.processes.get(&control.call_id).await.ok_or_else(|| {
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
    call_id: String,
    effect: EffectId,
    cancel: watch::Receiver<bool>,
    live: Arc<AtomicBool>,
    cas: Box<dyn CasSink>,
    output: Arc<dyn CommandOutputSink>,
    bounds: ProcessBounds,
}

#[derive(Debug)]
enum Captured {
    Chunk(OutputStream, Vec<u8>),
    ReadError(OutputStream, std::io::Error),
}

async fn supervise_process(supervisor: Supervisor) -> ToolResult<ProcessResult> {
    let Supervisor {
        mut child,
        stdout,
        stderr,
        stdin,
        pid,
        call_id,
        effect,
        mut cancel,
        live,
        mut cas,
        output,
        bounds,
    } = supervisor;
    let (captured_sender, mut captured) = mpsc::channel(32);
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

    let mut transcript = Vec::new();
    let mut output_bytes = 0usize;
    let mut fatal = None;
    let mut cancelled = *cancel.borrow();
    let mut cancel_open = true;
    let mut output_open = true;
    let mut exit_status = None;
    let mut wait_open = true;
    let mut kill_deadline: Option<Pin<Box<Sleep>>> = None;
    let mut wait = Box::pin(child.wait());

    if cancelled {
        let _ = signal_group(pid, Signal::TERM);
        kill_deadline = Some(Box::pin(sleep(bounds.kill_grace)));
    }

    while wait_open || output_open || kill_deadline.is_some() {
        tokio::select! {
            biased;
            changed = cancel.changed(), if !cancelled && cancel_open => {
                match changed {
                    Ok(()) if *cancel.borrow() => {
                        cancelled = true;
                        let _ = signal_group(pid, Signal::TERM);
                        kill_deadline = Some(Box::pin(sleep(bounds.kill_grace)));
                    }
                    Ok(()) => {}
                    Err(_) => cancel_open = false,
                }
            }
            maybe_chunk = captured.recv(), if output_open => {
                match maybe_chunk {
                    Some(Captured::Chunk(stream, bytes)) => {
                        output_bytes = output_bytes.saturating_add(bytes.len());
                        let chunk_b64 = BASE64.encode(&bytes);
                        if let Err(error) = output.emit(
                            &call_id,
                            ItemDelta::CommandOutput {
                                stream,
                                chunk_b64: chunk_b64.clone(),
                            },
                        ).await {
                            fatal.get_or_insert(error);
                            cancelled = false;
                            let _ = signal_group(pid, Signal::TERM);
                            kill_deadline = Some(Box::pin(sleep(bounds.kill_grace)));
                        }
                        transcript.push(ProcessOutputChunk { stream, chunk_b64 });
                    }
                    Some(Captured::ReadError(stream, error)) => {
                        fatal.get_or_insert_with(|| ToolError::Runtime {
                            message: format!("read {stream:?} from process `{call_id}`: {error}"),
                        });
                        cancelled = false;
                        let _ = signal_group(pid, Signal::TERM);
                        kill_deadline = Some(Box::pin(sleep(bounds.kill_grace)));
                    }
                    None => output_open = false,
                }
            }
            status = &mut wait, if wait_open => {
                wait_open = false;
                live.store(false, Ordering::Release);
                *stdin.lock().await = None;
                match status {
                    Ok(status) => exit_status = Some(status),
                    Err(error) => {
                        fatal.get_or_insert_with(|| ToolError::Runtime {
                            message: format!("wait for process `{call_id}`: {error}"),
                        });
                    }
                }
                if kill_deadline.is_some() && !process_group_exists(pid) {
                    kill_deadline = None;
                }
            }
            () = async {
                if let Some(deadline) = kill_deadline.as_mut() {
                    deadline.await;
                }
            }, if kill_deadline.is_some() => {
                if process_group_exists(pid) {
                    let _ = signal_group(pid, Signal::KILL);
                }
                kill_deadline = None;
            }
        }
    }

    if let Some(error) = fatal {
        return Err(error);
    }
    let Some(status) = exit_status else {
        return Err(ToolError::Runtime {
            message: format!("process `{call_id}` ended without an exit status"),
        });
    };
    let artifact = if output_bytes > bounds.max_inline_bytes {
        let frozen = serde_json::to_vec(&transcript).map_err(|error| ToolError::Runtime {
            message: format!("serialize process transcript: {error}"),
        })?;
        Some(cas.put(&frozen).await?)
    } else {
        None
    };
    let inline_output = if artifact.is_some() {
        Vec::new()
    } else {
        transcript
    };
    Ok(ProcessResult {
        call_id,
        effect,
        status: if cancelled {
            ToolStatus::Cancelled
        } else if status.success() {
            ToolStatus::Completed
        } else {
            ToolStatus::Failed
        },
        exit_code: status.code(),
        output_bytes,
        inline_output,
        artifact,
    })
}

async fn read_output<R>(mut reader: R, stream: OutputStream, sender: mpsc::Sender<Captured>)
where
    R: AsyncRead + Unpin,
{
    let mut buffer = vec![0_u8; OUTPUT_CHUNK_BYTES];
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
        ProcessControlAction::SendSignal(signal) => signal_group(active.pid, signal.rustix()),
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

fn signal_group(pid: Pid, signal: Signal) -> ToolResult<()> {
    kill_process_group(pid, signal).map_err(|error| ToolError::Runtime {
        message: format!(
            "send signal {signal:?} to process group {}: {error}",
            pid.as_raw_nonzero()
        ),
    })
}

fn process_group_exists(pid: Pid) -> bool {
    test_kill_process_group(pid).is_ok()
}

fn process_arguments(command: &str, cwd: &Path, env_allowlist: &[String]) -> ToolResult<Value> {
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
