//! Long-lived background shell tasks (W-A).
//!
//! [`EffectBroker::process_exec_background`] is the ONE spawn seam for
//! detached tasks. It reuses the hardened foreground confinement — the
//! anchored cwd re-walk, `env_clear`, and an own process group so kill is a
//! pgid kill — and adds the gate52 fd close-sweep: a background child
//! outlives the turn, so an accidentally inherited descriptor outlives the
//! caller's expectations with it (the daemon auto-spawn lesson).
//!
//! The broker effect terminalizes at successful spawn (the `spawn_subagent`
//! precedent): supervision transfers to the session-scoped registry, the
//! child is never entered into the broker's foreground [`ProcessRegistry`],
//! and broker close at turn end therefore never touches it — outliving the
//! turn is the feature, not a leak.

use crate::broker::{EffectBroker, EffectOperation, PermissionPolicy};
use crate::process::{
    Captured, PreparedProcessExec, ProcessBounds, ProcessExec, begin_group_termination,
    observe_process_leader_exit, process_arguments, process_group_exists, read_output,
    reap_process_leader, set_anchored_current_dir, signal_group, signal_group_for_sweep,
};
use crate::{ToolError, ToolResult};
use haider_protocol::ids::EffectId;
use haider_protocol::item::OutputStream;
use rustix::process::{Pid, Signal};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::env;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::process::{Child, ChildStderr, ChildStdout, Command};
use tokio::sync::{mpsc, watch};
use tokio::time::{Sleep, sleep};

/// Grace between the group TERM and the KILL escalation for task kills.
pub const BACKGROUND_KILL_GRACE: Duration = Duration::from_secs(2);

/// Hard cap on a task display name.
pub const MAX_TASK_NAME_CHARS: usize = 80;

/// Derives the default display name from the command's first token.
#[must_use]
pub fn default_task_name(command: &str) -> String {
    let token = command.split_whitespace().next().unwrap_or("task");
    let base = token.rsplit('/').next().unwrap_or(token);
    let name: String = base.chars().take(MAX_TASK_NAME_CHARS).collect();
    if name.is_empty() { "task".into() } else { name }
}

/// One background execution request: the confined operation plus its
/// display name. `background: true` joins the canonical argument digest, so
/// an approval for a foreground shape never silently covers the detached
/// shape (and vice versa); the display name deliberately does not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundExec {
    operation: ProcessExec,
    name: String,
}

impl BackgroundExec {
    pub fn new(operation: ProcessExec, name: impl Into<String>) -> ToolResult<Self> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(ToolError::invalid_argument(
                "background task name must not be empty",
            ));
        }
        if name.chars().count() > MAX_TASK_NAME_CHARS {
            return Err(ToolError::invalid_argument(format!(
                "background task name must be at most {MAX_TASK_NAME_CHARS} characters"
            )));
        }
        Ok(Self { operation, name })
    }

    pub fn operation(&self) -> &ProcessExec {
        &self.operation
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

fn background_arguments(operation: &ProcessExec) -> ToolResult<Value> {
    let mut arguments =
        process_arguments(&operation.command, &operation.cwd, &operation.env_allowlist)?;
    arguments["background"] = Value::Bool(true);
    Ok(arguments)
}

impl EffectOperation for BackgroundExec {
    fn effect_class(&self) -> haider_protocol::effect::EffectClass {
        haider_protocol::effect::EffectClass::ProcessExec
    }

    fn summary(&self) -> String {
        format!(
            "run background task `{}`: {}",
            self.name, self.operation.command
        )
    }

    fn arguments(&self) -> ToolResult<Value> {
        let mut arguments = background_arguments(&self.operation)?;
        arguments["name"] = Value::String(self.name.clone());
        Ok(arguments)
    }

    fn canonical_arguments(&self, workspace_root: &Path) -> ToolResult<Value> {
        background_arguments(&self.operation.resolved(workspace_root)?)
    }

    fn approval_preview(&self) -> Vec<String> {
        vec![
            format!(
                "Exact command: {}",
                serde_json::to_string(&self.operation.command)
                    .unwrap_or_else(|_| format!("{:?}", self.operation.command))
            ),
            format!("Working directory: {}", self.operation.cwd.display()),
            "Background: detached from the turn; completion reports as a session message".into(),
        ]
    }
}

struct PreparedBackgroundExec<'a> {
    prepared: &'a PreparedProcessExec,
    name: &'a str,
}

impl EffectOperation for PreparedBackgroundExec<'_> {
    fn effect_class(&self) -> haider_protocol::effect::EffectClass {
        haider_protocol::effect::EffectClass::ProcessExec
    }

    fn summary(&self) -> String {
        format!(
            "run background task `{}`: {} [detached; output cap {} bytes]",
            self.name,
            self.prepared.operation.command,
            haider_protocol::task::TASK_OUTPUT_RETAIN_BYTES,
        )
    }

    fn arguments(&self) -> ToolResult<Value> {
        let mut arguments = background_arguments(&self.prepared.operation)?;
        arguments["name"] = Value::String(self.name.to_owned());
        Ok(arguments)
    }

    fn canonical_arguments(&self, _workspace_root: &Path) -> ToolResult<Value> {
        background_arguments(&self.prepared.operation)
    }

    fn approval_preview(&self) -> Vec<String> {
        vec![
            format!(
                "Exact command: {}",
                serde_json::to_string(&self.prepared.operation.command)
                    .unwrap_or_else(|_| format!("{:?}", self.prepared.operation.command))
            ),
            format!(
                "Working directory: {}",
                self.prepared.operation.cwd.display()
            ),
            "Background: detached from the turn; completion reports as a session message".into(),
        ]
    }
}

/// A freshly spawned, unsupervised background child. The caller must hand it
/// to [`supervise_background`] promptly; dropping it leaks the child until
/// the session-close fence.
pub struct BackgroundSpawn {
    pub call_id: String,
    pub effect: EffectId,
    /// Process-group leader pid (`pgid == pid`).
    pub pid: i32,
    child: Child,
    stdout: ChildStdout,
    stderr: ChildStderr,
    pid_handle: Pid,
}

impl std::fmt::Debug for BackgroundSpawn {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BackgroundSpawn")
            .field("call_id", &self.call_id)
            .field("effect", &self.effect)
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

impl EffectBroker {
    /// Spawns a long-lived background command through the hardened path.
    ///
    /// The four effect phases journal here and the effect terminalizes at
    /// the spawn boundary: `Ok` means the child is alive in its own process
    /// group. The child is deliberately NOT registered in the foreground
    /// process registry — broker close (turn end, esc) must never cancel it.
    pub async fn process_exec_background(
        &mut self,
        operation: &BackgroundExec,
        policy: &PermissionPolicy,
    ) -> ToolResult<BackgroundSpawn> {
        let prepared = PreparedProcessExec::new(
            operation.operation(),
            self.workspace_root(),
            self.duplicate_workspace_dir()?,
            ProcessBounds::default(),
        )?;
        let resolved = prepared.operation.clone();
        let intent = self
            .begin(
                &PreparedBackgroundExec {
                    prepared: &prepared,
                    name: operation.name(),
                },
                policy,
            )
            .await?;
        let cwd_fd =
            match prepared.cwd_for_spawn(self.workspace_root(), self.duplicate_workspace_dir()?) {
                Ok(cwd_fd) => cwd_fd,
                Err(error) => return self.finish(&intent, Err(error)).await,
            };
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(&resolved.command)
            .env_clear()
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        for name in &resolved.env_allowlist {
            if let Some(value) = env::var_os(name) {
                command.env(name, value);
            }
        }
        set_anchored_current_dir(&mut command, cwd_fd);
        command.as_std_mut().process_group(0);
        close_inherited_descriptors(&mut command);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return self
                    .finish(
                        &intent,
                        Err(ToolError::io("spawn background task", &resolved.cwd, error)),
                    )
                    .await;
            }
        };
        let pid_raw = child.id().and_then(|raw| i32::try_from(raw).ok());
        let Some(pid_handle) = pid_raw.and_then(Pid::from_raw) else {
            if let Some(id) = child.id().and_then(|raw| i32::try_from(raw).ok())
                && let Some(pid) = Pid::from_raw(id)
            {
                let _ = signal_group(pid, Signal::KILL);
            }
            return self
                .finish(
                    &intent,
                    Err(ToolError::Runtime {
                        message: "spawned background task did not expose a process id".into(),
                    }),
                )
                .await;
        };
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (Some(stdout), Some(stderr)) = (stdout, stderr) else {
            let _ = signal_group(pid_handle, Signal::KILL);
            return self
                .finish(
                    &intent,
                    Err(ToolError::Runtime {
                        message: "spawned background task output pipes are unavailable".into(),
                    }),
                )
                .await;
        };
        let spawn = BackgroundSpawn {
            call_id: resolved.call_id.clone(),
            effect: intent.effect.clone(),
            pid: pid_handle.as_raw_nonzero().get(),
            child,
            stdout,
            stderr,
            pid_handle,
        };
        self.finish(&intent, Ok(spawn)).await
    }
}

/// gate52 fd hygiene for children that outlive their spawner's expectations:
/// close every descriptor above stderr between fork and exec. macOS creates
/// pipes without atomic CLOEXEC, so a concurrent spawn on another thread can
/// capture a sibling's descriptor; a leaked pipe write end held by a
/// long-lived task starves the original reader of EOF forever.
///
/// Registered AFTER the anchored `fchdir` hook, so the cwd fd has already
/// served its purpose when it is closed.
#[allow(unsafe_code)]
fn close_inherited_descriptors(command: &mut Command) {
    // SAFETY: the hook runs between fork and exec and calls only the
    // async-signal-safe `close(2)`; no allocation, locking, or Rust runtime
    // machinery is touched.
    unsafe {
        use std::os::unix::process::CommandExt as _;
        command.as_std_mut().pre_exec(|| {
            for fd in 3..65_536_i32 {
                rustix::io::close(fd);
            }
            Ok(())
        });
    }
}

/// Bounded combined stdout+stderr for one background task: a retained head
/// (cap [`haider_protocol::task::TASK_OUTPUT_RETAIN_BYTES`]) for cursor
/// reads and the completion artifact, a rolling tail preview, and a total
/// byte counter. Output beyond the cap is dropped — never buffered — so
/// memory stays bounded while the task keeps running.
#[derive(Debug)]
pub struct TaskOutputBuffer {
    retained: Vec<u8>,
    retain_cap: usize,
    tail: VecDeque<u8>,
    tail_cap: usize,
    total: u64,
}

impl TaskOutputBuffer {
    #[must_use]
    pub fn new(retain_cap: usize, tail_cap: usize) -> Self {
        Self {
            retained: Vec::new(),
            retain_cap,
            tail: VecDeque::new(),
            tail_cap,
            total: 0,
        }
    }

    pub fn append(&mut self, bytes: &[u8]) {
        self.total = self.total.saturating_add(bytes.len() as u64);
        let room = self.retain_cap.saturating_sub(self.retained.len());
        self.retained
            .extend_from_slice(&bytes[..bytes.len().min(room)]);
        self.tail.extend(bytes.iter().copied());
        while self.tail.len() > self.tail_cap {
            self.tail.pop_front();
        }
    }

    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.total
    }

    /// True when output beyond the retained cap was dropped.
    #[must_use]
    pub fn truncated(&self) -> bool {
        self.total > self.retained.len() as u64
    }

    #[must_use]
    pub fn retained(&self) -> &[u8] {
        &self.retained
    }

    /// Last-`tail_cap` bytes as lossy UTF-8.
    #[must_use]
    pub fn tail_lossy(&self) -> String {
        let (front, back) = self.tail.as_slices();
        let mut bytes = Vec::with_capacity(self.tail.len());
        bytes.extend_from_slice(front);
        bytes.extend_from_slice(back);
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// Reads up to `max` retained bytes from `cursor`, returning the chunk
    /// and the next cursor. A cursor past the retained region yields an
    /// empty chunk (the drop beyond the cap is visible via
    /// [`Self::truncated`], never silently rewound).
    #[must_use]
    pub fn read_from(&self, cursor: u64, max: usize) -> (Vec<u8>, u64) {
        let start = usize::try_from(cursor.min(self.retained.len() as u64)).unwrap_or(usize::MAX);
        let start = start.min(self.retained.len());
        let end = start.saturating_add(max).min(self.retained.len());
        (self.retained[start..end].to_vec(), end as u64)
    }
}

/// Shared handle to one task's live output.
pub type SharedTaskOutput = Arc<Mutex<TaskOutputBuffer>>;

#[must_use]
pub fn shared_task_output(retain_cap: usize, tail_cap: usize) -> SharedTaskOutput {
    Arc::new(Mutex::new(TaskOutputBuffer::new(retain_cap, tail_cap)))
}

/// Locks a shared output buffer, tolerating a poisoned writer.
pub fn lock_task_output(output: &SharedTaskOutput) -> std::sync::MutexGuard<'_, TaskOutputBuffer> {
    output.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Cloneable kill capability for one supervised background task. It can only
/// request the supervised TERM → grace → KILL ladder; it cannot inspect or
/// forge a result.
#[derive(Clone, Debug)]
pub struct TaskKillHandle {
    sender: watch::Sender<bool>,
}

impl TaskKillHandle {
    pub fn kill(&self) {
        self.sender.send_replace(true);
    }
}

#[must_use]
pub fn task_kill_channel() -> (TaskKillHandle, watch::Receiver<bool>) {
    let (sender, receiver) = watch::channel(false);
    (TaskKillHandle { sender }, receiver)
}

/// Terminal observation of one supervised background task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundExitStatus {
    /// Leader exit code; `None` when it was ended by a signal.
    pub exit_code: Option<i32>,
    /// True when the kill ladder was requested before the leader exited.
    pub killed: bool,
    /// Supervision fault detail (probe/signal/reap errors), if any.
    pub fault: Option<String>,
}

/// Supervises one background child to its terminal observation: pipes tee
/// into the bounded buffer, the leader exit is observed without reaping
/// (`WNOWAIT`) so the zombie pins the pgid through the group sweep, the
/// group is swept TERM → grace → KILL (on kill request AND on natural exit,
/// so lingering descendants never outlive the task), and the leader is
/// reaped last. This mirrors the foreground supervisor's ordering laws with
/// the turn-scoped concerns (wall timeout, cancel watch, output-cap kill)
/// deliberately absent: a background task has no wall clock and output
/// beyond the cap is dropped, not fatal.
pub async fn supervise_background(
    spawn: BackgroundSpawn,
    mut kill: watch::Receiver<bool>,
    output: SharedTaskOutput,
    grace: Duration,
) -> BackgroundExitStatus {
    let BackgroundSpawn {
        call_id,
        effect: _,
        pid: _,
        mut child,
        stdout,
        stderr,
        pid_handle: pid,
    } = spawn;
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

    let stdin = Arc::new(tokio::sync::Mutex::new(None));
    let mut fatal: Option<ToolError> = None;
    let mut escalation_notes = Vec::new();
    let mut lifecycle_events = Vec::new();
    let mut killed = false;
    let mut kill_open = true;
    let mut output_open = true;
    let mut exit_status = None;
    let mut leader_exit_observed = false;
    let mut leader_is_zombie = false;
    let mut leader_reaped = false;
    let mut kill_deadline: Option<Pin<Box<Sleep>>> = None;
    let mut pipe_drain_deadline: Option<Pin<Box<Sleep>>> = None;
    let mut exit_observation = Box::pin(observe_process_leader_exit(pid));

    if *kill.borrow() {
        killed = true;
        begin_group_termination(
            pid,
            false,
            grace,
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
            changed = kill.changed(), if kill_open && !killed => {
                match changed {
                    Ok(()) if *kill.borrow() => {
                        killed = !leader_exit_observed;
                        begin_group_termination(
                            pid,
                            leader_is_zombie,
                            grace,
                            &mut kill_deadline,
                            &mut fatal,
                            &mut escalation_notes,
                            &mut lifecycle_events,
                        );
                    }
                    Ok(()) => {}
                    Err(_) => kill_open = false,
                }
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
                    Some(Captured::Chunk(_, bytes)) => {
                        lock_task_output(&output).append(&bytes);
                    }
                    Some(Captured::ReadError(stream, error)) => {
                        fatal.get_or_insert_with(|| ToolError::Runtime {
                            message: format!(
                                "read {stream:?} from background task `{call_id}`: {error}"
                            ),
                        });
                    }
                    None => {
                        output_open = false;
                        pipe_drain_deadline = None;
                    }
                }
            }
            observed = &mut exit_observation, if !leader_exit_observed => {
                leader_exit_observed = true;
                leader_is_zombie = match observed {
                    Ok(()) => true,
                    Err(error) => {
                        fatal.get_or_insert(error);
                        false
                    }
                };
                if kill_deadline.is_none() {
                    begin_group_termination(
                        pid,
                        leader_is_zombie,
                        grace,
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
                    leader_reaped = true;
                    if output_open {
                        pipe_drain_deadline = Some(Box::pin(sleep(grace)));
                    }
                }
            }
            () = async {
                if let Some(deadline) = kill_deadline.as_mut() {
                    deadline.await;
                }
            }, if kill_deadline.is_some() => {
                match process_group_exists(pid) {
                    Ok(false) => {
                        kill_deadline = None;
                    }
                    Ok(true) => {
                        if let Err(error) =
                            signal_group_for_sweep(pid, Signal::KILL, leader_is_zombie)
                        {
                            escalation_notes.push(format!(
                                "SIGKILL escalation failed for background task group {}: {error}",
                                pid.as_raw_nonzero()
                            ));
                            fatal.get_or_insert(error);
                            break;
                        }
                        kill_deadline = None;
                    }
                    Err(error) => {
                        escalation_notes.push(format!(
                            "process-group probe failed during background SIGKILL \
                             escalation: {error}"
                        ));
                        fatal.get_or_insert(error);
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
                    leader_reaped = true;
                    if output_open {
                        pipe_drain_deadline = Some(Box::pin(sleep(grace)));
                    }
                }
            }
        }
    }

    if exit_status.is_none() && !killed {
        fatal.get_or_insert_with(|| ToolError::Runtime {
            message: format!("background task `{call_id}` ended without an exit status"),
        });
    }
    let mut fault_parts: Vec<String> = escalation_notes;
    if let Some(error) = &fatal {
        fault_parts.push(error.to_string());
    }
    BackgroundExitStatus {
        exit_code: exit_status
            .as_ref()
            .and_then(std::process::ExitStatus::code),
        killed,
        fault: (!fault_parts.is_empty()).then(|| fault_parts.join("; ")),
    }
}

/// Liveness verdict for one orphan-candidate process group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PidLiveness {
    Alive,
    Dead,
}

/// Outcome of one orphan process-group reap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrphanReap {
    AlreadyDead,
    Killed,
    Failed { message: String },
}

/// Real pgid liveness probe (`killpg(pgid, 0)`); tests inject fakes.
#[must_use]
pub fn probe_group_liveness(pid_raw: i32) -> PidLiveness {
    match Pid::from_raw(pid_raw) {
        Some(pid) => match process_group_exists(pid) {
            Ok(true) => PidLiveness::Alive,
            // A probe failure is treated as alive: reaping then fails loudly
            // instead of silently forgetting a possibly-live group.
            Ok(false) => PidLiveness::Dead,
            Err(_) => PidLiveness::Alive,
        },
        None => PidLiveness::Dead,
    }
}

/// Reaps one stale background process group after a daemon restart: probe
/// liveness through the injected seam, then TERM → grace → KILL the pgid.
/// The orphan was reparented when the previous daemon died, so `ESRCH` at
/// any rung means the group is gone. Both rungs pass the zombie-tolerant
/// sweep flag: an `EPERM` here can only mean an unreaped same-pgid zombie
/// (a caller-owned child in tests) — a live foreign-uid group was already
/// impossible for a group the previous daemon spawned.
pub async fn reap_orphan_group<P>(pid_raw: i32, grace: Duration, probe: P) -> OrphanReap
where
    P: Fn(i32) -> PidLiveness,
{
    if probe(pid_raw) == PidLiveness::Dead {
        return OrphanReap::AlreadyDead;
    }
    let Some(pid) = Pid::from_raw(pid_raw) else {
        return OrphanReap::Failed {
            message: format!("orphan pid {pid_raw} is not representable"),
        };
    };
    match signal_group_for_sweep(pid, Signal::TERM, true) {
        Ok(true) => {}
        Ok(false) => return OrphanReap::AlreadyDead,
        Err(error) => {
            return OrphanReap::Failed {
                message: format!("orphan TERM failed: {error}"),
            };
        }
    }
    sleep(grace).await;
    match process_group_exists(pid) {
        Ok(false) => OrphanReap::Killed,
        Ok(true) => match signal_group_for_sweep(pid, Signal::KILL, true) {
            Ok(_) => OrphanReap::Killed,
            Err(error) => OrphanReap::Failed {
                message: format!("orphan KILL failed: {error}"),
            },
        },
        Err(error) => OrphanReap::Failed {
            message: format!("orphan probe failed after TERM: {error}"),
        },
    }
}

/// Task tool manifests (registered by the daemon's single tool registry).
#[must_use]
pub fn task_output_manifest() -> haider_protocol::tool::ToolManifest {
    haider_protocol::tool::ToolManifest {
        name: "task_output".into(),
        description: "Read bounded output from a background task started with \
                      process_exec background=true. Without a cursor it returns the \
                      rolling tail preview; with a cursor it pages the retained output."
            .into(),
        effects: vec![],
        dispatch: haider_protocol::tool::DispatchMode::Await,
        input_schema: json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Task id returned by the background process_exec call"
                },
                "cursor": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Optional byte offset into the retained output"
                }
            },
            "required": ["task_id"],
            "additionalProperties": false,
        }),
    }
}

#[must_use]
pub fn task_kill_manifest() -> haider_protocol::tool::ToolManifest {
    haider_protocol::tool::ToolManifest {
        name: "task_kill".into(),
        description: "Terminate a background task's whole process group \
                      (TERM, then KILL after a grace period)."
            .into(),
        effects: vec![haider_protocol::effect::EffectClass::ProcessExec],
        dispatch: haider_protocol::tool::DispatchMode::Await,
        input_schema: json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Task id returned by the background process_exec call"
                }
            },
            "required": ["task_id"],
            "additionalProperties": false,
        }),
    }
}
