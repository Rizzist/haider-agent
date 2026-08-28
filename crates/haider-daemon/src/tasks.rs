//! Session-scoped background task coordination (W-A).
//!
//! The JOURNAL is the truth: `task_started` / `task_completed` facts (the
//! additive [`TaskEventPayload`] union) are appended on the session's own
//! timeline, and [`TaskRegistry`] — held by the hub — is an in-memory
//! projection rebuilt lazily per session ([`TaskFacade::adopt_session`]).
//! Re-adoption after a daemon restart reaps stale process groups through an
//! injectable pid-liveness seam and journals the orphaned completion
//! honestly.
//!
//! Completion is a SESSION MESSAGE: the completed fact renders as a
//! transcript row and (when no run was steered) carries the bounded prompt
//! notice for the next turn; when a run is ACTIVE the same notice is
//! delivered as a durable mid-turn STEER through the existing nudge
//! machinery (the S1 `message_subagent` precedent), and the fact then
//! journals with `PromptRender::Omit` so exactly one prompt copy exists.

use crate::session_hub::SessionHub;
use haider_core::{TurnAcceptCommand, TurnAdmissionDisposition, TurnCancelCommand};
use haider_protocol::DeliveryMode;
use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION,
};
use haider_protocol::error::HaiderError;
use haider_protocol::ids::{AgentId, BranchId, EventId, RunId, SessionId, TaskId};
use haider_protocol::state::RunState;
use haider_protocol::task::{
    TASK_CONCURRENCY_CAP, TASK_OUTPUT_RETAIN_BYTES, TASK_TAIL_BYTES, TaskCompleted,
    TaskCompletionDelivery, TaskEventPayload, TaskStarted, TaskTerminalState,
};
use haider_protocol::tool::{BoundedResult, ToolResultStatus};
use haider_tools::{
    BACKGROUND_KILL_GRACE, BackgroundExec, BackgroundExitStatus, EffectBroker, EffectOperation,
    PermissionPolicy, PidLiveness, ProcessExec, SharedTaskOutput, TaskKillHandle, ToolError,
    ToolResult, default_task_name, lock_task_output, probe_group_liveness, reap_orphan_group,
    shared_task_output, supervise_background, task_kill_channel,
};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Bounded command summary carried by the started fact (display only).
const TASK_COMMAND_SUMMARY_CHARS: usize = 200;
/// Bounded failure-reason detail carried by a failed completion.
const TASK_FAILURE_REASON_CHARS: usize = 400;
/// One `task_output` cursor read returns at most this many bytes.
pub(crate) const TASK_OUTPUT_READ_BYTES: usize = 8 * 1024;
/// Kill settles when the supervised ladder reports terminal within this
/// margin past TERM + grace + KILL.
const KILL_SETTLE_MARGIN: Duration = Duration::from_secs(3);
const KILL_SETTLE_POLL: Duration = Duration::from_millis(25);

/// Live/terminal projection state of one task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TaskLiveState {
    Running,
    Terminal(TaskTerminalState),
}

/// One session's task, as projected into the registry.
#[derive(Clone)]
pub(crate) struct TaskEntry {
    pub(crate) task: TaskId,
    pub(crate) name: String,
    pub(crate) pid: i32,
    pub(crate) started_at_ms: u64,
    pub(crate) state: TaskLiveState,
    run_id: RunId,
    branch_id: Option<BranchId>,
    agent_id: Option<AgentId>,
    /// Present while output has no durable backing. Successful CAS storage
    /// atomically replaces this allocation with `terminal_fact.artifact`.
    output: Option<SharedTaskOutput>,
    /// `None` for entries re-adopted from a prior daemon life (their
    /// supervision — and live output — died with that daemon).
    kill: Option<TaskKillHandle>,
    /// Staged as soon as CAS succeeds so reads can cross the live-to-durable
    /// swap; its delivery field is finalized when the state becomes terminal.
    terminal_fact: Option<TaskCompleted>,
}

#[derive(Default)]
struct SessionTasks {
    adopted: bool,
    tasks: HashMap<TaskId, TaskEntry>,
}

/// In-memory projection of every session's background tasks (hub-owned).
#[derive(Default)]
pub(crate) struct TaskRegistry {
    sessions: StdMutex<HashMap<SessionId, SessionTasks>>,
}

impl TaskRegistry {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<SessionId, SessionTasks>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Marks the session adopted; returns whether THIS caller owns adoption.
    fn begin_adoption(&self, session_id: &SessionId) -> bool {
        let mut sessions = self.lock();
        let session = sessions.entry(session_id.clone()).or_default();
        if session.adopted {
            return false;
        }
        session.adopted = true;
        true
    }

    fn abandon_adoption(&self, session_id: &SessionId) {
        if let Some(session) = self.lock().get_mut(session_id) {
            session.adopted = false;
        }
    }

    fn insert(&self, session_id: &SessionId, entry: TaskEntry) {
        self.lock()
            .entry(session_id.clone())
            .or_default()
            .tasks
            .insert(entry.task.clone(), entry);
    }

    fn insert_if_absent(&self, session_id: &SessionId, entry: TaskEntry) {
        self.lock()
            .entry(session_id.clone())
            .or_default()
            .tasks
            .entry(entry.task.clone())
            .or_insert(entry);
    }

    pub(crate) fn get(&self, session_id: &SessionId, task: &TaskId) -> Option<TaskEntry> {
        self.lock()
            .get(session_id)
            .and_then(|session| session.tasks.get(task))
            .cloned()
    }

    fn known(&self, session_id: &SessionId, task: &TaskId) -> bool {
        self.lock()
            .get(session_id)
            .is_some_and(|session| session.tasks.contains_key(task))
    }

    fn set_terminal(
        &self,
        session_id: &SessionId,
        task: &TaskId,
        state: TaskTerminalState,
        fact: TaskCompleted,
    ) {
        if let Some(entry) = self
            .lock()
            .get_mut(session_id)
            .and_then(|session| session.tasks.get_mut(task))
        {
            entry.state = TaskLiveState::Terminal(state);
            entry.terminal_fact = Some(fact);
            entry.kill = None;
        }
    }

    /// Switches completed output to its durable backing in one registry
    /// step. Readers see either the intact live buffer or the CAS reference,
    /// never an evicted buffer without a source for cursor pages.
    fn stage_durable_output(&self, session_id: &SessionId, task: &TaskId, fact: &TaskCompleted) {
        if fact.artifact.is_none() {
            return;
        }
        if let Some(entry) = self
            .lock()
            .get_mut(session_id)
            .and_then(|session| session.tasks.get_mut(task))
            .filter(|entry| entry.state == TaskLiveState::Running)
        {
            entry.terminal_fact = Some(fact.clone());
            entry.output = None;
        }
    }

    pub(crate) fn running_count(&self, session_id: &SessionId) -> usize {
        self.lock().get(session_id).map_or(0, |session| {
            session
                .tasks
                .values()
                .filter(|entry| entry.state == TaskLiveState::Running)
                .count()
        })
    }

    fn running_entries(&self, session_id: &SessionId) -> Vec<TaskEntry> {
        self.lock()
            .get(session_id)
            .map_or_else(Vec::new, |session| {
                session
                    .tasks
                    .values()
                    .filter(|entry| entry.state == TaskLiveState::Running)
                    .cloned()
                    .collect()
            })
    }

    /// Removes and returns the whole session projection (delete fence).
    fn remove_session(&self, session_id: &SessionId) -> Vec<TaskEntry> {
        self.lock()
            .remove(session_id)
            .map_or_else(Vec::new, |session| session.tasks.into_values().collect())
    }

    fn sessions_with_running(&self) -> Vec<SessionId> {
        self.lock()
            .iter()
            .filter(|(_, session)| {
                session
                    .tasks
                    .values()
                    .any(|entry| entry.state == TaskLiveState::Running)
            })
            .map(|(session_id, _)| session_id.clone())
            .collect()
    }
}

/// The one `task_kill` control effect (existing process ceiling).
struct TaskKillEffect {
    task: TaskId,
    name: String,
    pid: i32,
}

impl EffectOperation for TaskKillEffect {
    fn effect_class(&self) -> haider_protocol::effect::EffectClass {
        haider_protocol::effect::EffectClass::ProcessExec
    }

    fn summary(&self) -> String {
        format!("kill background task `{}` ({})", self.name, self.task)
    }

    fn arguments(&self) -> ToolResult<serde_json::Value> {
        Ok(json!({
            "task_id": self.task,
            "pid": self.pid,
        }))
    }
}

/// Worker-side coordinates for one background spawn.
pub(crate) struct TaskSpawnContext {
    pub(crate) session_id: SessionId,
    pub(crate) run_id: RunId,
    pub(crate) branch_id: Option<BranchId>,
    pub(crate) agent_id: Option<AgentId>,
    pub(crate) call_id: String,
}

/// Stateless handle over the hub-owned registry (the delegation pattern):
/// every clone shares the ONE projection inside the hub.
#[derive(Clone)]
pub(crate) struct TaskFacade {
    hub: SessionHub,
    kill_grace: Duration,
}

impl TaskFacade {
    pub(crate) fn new(hub: SessionHub) -> Self {
        Self {
            hub,
            kill_grace: BACKGROUND_KILL_GRACE,
        }
    }

    #[cfg(all(test, unix))]
    pub(crate) fn with_kill_grace(hub: SessionHub, kill_grace: Duration) -> Self {
        Self { hub, kill_grace }
    }

    /// Rebuilds one session's registry projection from its journal, exactly
    /// once per daemon life, reaping orphans through the REAL liveness probe.
    pub(crate) async fn adopt_session(&self, session_id: &SessionId) -> Result<(), HaiderError> {
        self.adopt_session_with_probe(session_id, probe_group_liveness)
            .await
    }

    /// LT6 seam: adoption with an injectable pid-liveness probe. A started
    /// fact without a completed fact is an orphan candidate; a live stale
    /// pgid is killed (TERM → grace → KILL) and the completion journaled
    /// honestly — the journal stays the truth across restarts.
    pub(crate) async fn adopt_session_with_probe<P>(
        &self,
        session_id: &SessionId,
        probe: P,
    ) -> Result<(), HaiderError>
    where
        P: Fn(i32) -> PidLiveness + Send + Sync,
    {
        let registry = self.hub.task_registry();
        if !registry.begin_adoption(session_id) {
            return Ok(());
        }
        let scan = match self.scan_session_tasks(session_id).await {
            Ok(scan) => scan,
            Err(error) => {
                registry.abandon_adoption(session_id);
                return Err(error);
            }
        };
        for (task, started) in scan.started {
            if registry.known(session_id, &task) {
                continue;
            }
            if let Some(completed) = scan.completed.get(&task) {
                let state = completed.state.clone();
                registry.insert_if_absent(
                    session_id,
                    TaskEntry {
                        task: task.clone(),
                        name: completed.name.clone(),
                        pid: started.fact.pid,
                        started_at_ms: started.fact.started_at_ms,
                        state: TaskLiveState::Terminal(state),
                        run_id: started.run_id.clone(),
                        branch_id: started.branch_id.clone(),
                        agent_id: started.agent_id.clone(),
                        output: completed
                            .artifact
                            .is_none()
                            .then(|| shared_task_output(0, 0)),
                        kill: None,
                        terminal_fact: Some(completed.clone()),
                    },
                );
                continue;
            }
            // Orphan: a prior daemon life started it and never completed it.
            let reap = reap_orphan_group(started.fact.pid, self.kill_grace, &probe).await;
            let reason = match reap {
                haider_tools::OrphanReap::AlreadyDead => {
                    "orphaned by daemon restart (process group already gone; output lost)"
                        .to_owned()
                }
                haider_tools::OrphanReap::Killed => {
                    "orphaned by daemon restart; stale process group reaped".to_owned()
                }
                haider_tools::OrphanReap::Failed { message } => bounded_chars(
                    &format!("orphaned by daemon restart; reap failed: {message}"),
                    TASK_FAILURE_REASON_CHARS,
                ),
            };
            let state = TaskTerminalState::Failed { reason };
            let completed = TaskCompleted {
                task: task.clone(),
                name: started.fact.name.clone(),
                state: state.clone(),
                elapsed_ms: now_ms().saturating_sub(started.fact.started_at_ms),
                output_bytes: 0,
                tail: String::new(),
                artifact: None,
                truncated: false,
                full_output_unavailable: false,
                delivery: TaskCompletionDelivery::DeliveredQueued,
                workspace_mutation: None,
            };
            let mut envelopes = [self.task_fact_envelope(
                session_id,
                &started.run_id,
                started.branch_id.as_ref(),
                started.agent_id.as_ref(),
                &format!("task-completed-{}", task.as_str()),
                completed
                    .to_payload_value()
                    .map_err(internal_serialization)?,
                PromptRender::Verbatim,
            )];
            self.hub.append(&mut envelopes).await?;
            registry.insert_if_absent(
                session_id,
                TaskEntry {
                    task: task.clone(),
                    name: started.fact.name.clone(),
                    pid: started.fact.pid,
                    started_at_ms: started.fact.started_at_ms,
                    state: TaskLiveState::Terminal(state),
                    run_id: started.run_id,
                    branch_id: started.branch_id,
                    agent_id: started.agent_id,
                    output: Some(shared_task_output(0, 0)),
                    kill: None,
                    terminal_fact: Some(completed),
                },
            );
        }
        Ok(())
    }

    /// Spawns one background task: cap check, the hardened broker spawn (the
    /// effect terminalizes at the spawn boundary), the durable started fact,
    /// registry insertion, and the detached supervision + completion
    /// pipeline. Returns IMMEDIATELY with the typed running result (LT1).
    pub(crate) async fn spawn_background(
        &self,
        context: TaskSpawnContext,
        command: String,
        cwd: Option<String>,
        name: Option<String>,
        broker: &mut EffectBroker,
        policy: &PermissionPolicy,
    ) -> ToolResult<BoundedResult> {
        self.adopt_session(&context.session_id)
            .await
            .map_err(runtime_tool_error)?;
        let registry = self.hub.task_registry();
        let running = registry.running_count(&context.session_id);
        if running >= TASK_CONCURRENCY_CAP {
            return Ok(BoundedResult {
                preview: json!({
                    "status": "refused",
                    "kind": "task_cap_reached",
                    "running": running,
                    "cap": TASK_CONCURRENCY_CAP,
                    "message": "background task cap reached; wait for a completion or task_kill one",
                })
                .to_string(),
                truncated: false,
                data: None,
                artifact: None,
                images: Vec::new(),
                cursor: None,
                status: ToolResultStatus::Rejected,
                reason: Some("background task concurrency cap reached".into()),
                presentation: None,
            });
        }
        let name = name.unwrap_or_else(|| default_task_name(&command));
        let mut operation = ProcessExec::new(context.call_id.clone(), command.clone());
        if let Some(cwd) = cwd.as_ref() {
            operation = operation.with_cwd(cwd.clone());
        }
        let operation = BackgroundExec::new(operation, name.clone())?;
        let spawn = broker.process_exec_background(&operation, policy).await?;
        let shell = match self.hub.shell_registry().open(
            haider_rpc::ShellKindWire::Local,
            name.clone(),
            cwd.unwrap_or_else(|| ".".into()),
        ) {
            Ok(shell) => shell,
            Err(error) => {
                let (kill, kill_signal) = task_kill_channel();
                kill.kill();
                tokio::spawn(supervise_background(
                    spawn,
                    kill_signal,
                    shared_task_output(0, 0),
                    self.kill_grace,
                ));
                return Err(ToolError::Runtime {
                    message: format!("cannot register background shell: {error}"),
                });
            }
        };
        if let Err(error) = shell.running() {
            let (kill, kill_signal) = task_kill_channel();
            kill.kill();
            tokio::spawn(supervise_background(
                spawn,
                kill_signal,
                shared_task_output(0, 0),
                self.kill_grace,
            ));
            return Err(ToolError::Runtime {
                message: format!("cannot start registered background shell: {error}"),
            });
        }
        let task = TaskId::new(format!(
            "task-{}",
            &crate::delegation::stable_digest(&[
                context.session_id.as_str(),
                context.run_id.as_str(),
                &context.call_id,
            ])[..16]
        ));
        let started_at_ms = now_ms();
        let pid = spawn.pid;
        let started = TaskStarted {
            task: task.clone(),
            name: name.clone(),
            command: bounded_chars(&command, TASK_COMMAND_SUMMARY_CHARS),
            pid,
            started_at_ms,
        };
        let mut envelopes = [self.task_fact_envelope(
            &context.session_id,
            &context.run_id,
            context.branch_id.as_ref(),
            context.agent_id.as_ref(),
            &format!("task-started-{}", task.as_str()),
            started
                .to_payload_value()
                .map_err(|error| runtime_tool_error(internal_serialization(error)))?,
            PromptRender::Verbatim,
        )];
        if let Err(error) = self.hub.append(&mut envelopes).await {
            // No durable record may mean a leaked child: kill through the
            // supervised ladder so the group is reaped, then fail honestly.
            let (kill, kill_signal) = task_kill_channel();
            kill.kill();
            tokio::spawn(supervise_background(
                spawn,
                kill_signal,
                shared_task_output(0, 0),
                self.kill_grace,
            ));
            let _ = shell.exited(None);
            return Err(runtime_tool_error(error));
        }
        let (kill, kill_signal) = task_kill_channel();
        let output = shared_task_output(TASK_OUTPUT_RETAIN_BYTES, TASK_TAIL_BYTES);
        registry.insert(
            &context.session_id,
            TaskEntry {
                task: task.clone(),
                name: name.clone(),
                pid,
                started_at_ms,
                state: TaskLiveState::Running,
                run_id: context.run_id.clone(),
                branch_id: context.branch_id.clone(),
                agent_id: context.agent_id.clone(),
                output: Some(Arc::clone(&output)),
                kill: Some(kill.clone()),
                terminal_fact: None,
            },
        );
        let facade = self.clone();
        let session_id = context.session_id.clone();
        let pipeline_task = task.clone();
        let output_for_shell = Arc::clone(&output);
        let supervision = supervise_background(spawn, kill_signal, output, self.kill_grace);
        tokio::spawn(async move {
            tokio::pin!(supervision);
            let mut close = shell.close_receiver();
            let mut close_open = true;
            let status = loop {
                tokio::select! {
                    status = &mut supervision => break status,
                    changed = close.changed(), if close_open => {
                        if changed.is_err() {
                            close_open = false;
                        } else if *close.borrow_and_update() {
                            kill.kill();
                        }
                    }
                }
            };
            let output_bytes = lock_task_output(&output_for_shell).total_bytes();
            let _ = shell.add_output(output_bytes);
            let _ = shell.exited(status.exit_code);
            facade
                .complete_task(&session_id, &pipeline_task, status)
                .await;
        });
        Ok(BoundedResult {
            preview: json!({
                "task_id": task,
                "name": name,
                "state": "running",
            })
            .to_string(),
            truncated: false,
            data: None,
            artifact: None,
            images: Vec::new(),
            cursor: None,
            status: ToolResultStatus::Completed,
            reason: None,
            presentation: None,
        })
    }

    /// Terminal half of the pipeline: bound the output into a CAS artifact,
    /// deliver the completion (steer when a run is active, otherwise the
    /// fact carries the next-turn prompt notice), journal the completed
    /// fact, and settle the registry projection.
    async fn complete_task(
        &self,
        session_id: &SessionId,
        task: &TaskId,
        status: BackgroundExitStatus,
    ) {
        let registry = self.hub.task_registry();
        let Some(mut entry) = registry.get(session_id, task) else {
            // The delete fence removed the session projection: the session
            // (and its journal) is gone — nothing to record.
            return;
        };
        let state = if status.killed {
            TaskTerminalState::Killed
        } else if let Some(fault) = status.fault.clone() {
            TaskTerminalState::Failed {
                reason: bounded_chars(&fault, TASK_FAILURE_REASON_CHARS),
            }
        } else if status.exit_code == Some(0) {
            TaskTerminalState::Completed { exit_code: Some(0) }
        } else if let Some(exit_code) = status.exit_code {
            TaskTerminalState::Failed {
                reason: format!("process exited with code {exit_code}"),
            }
        } else if let Some(signal) = status.signal {
            TaskTerminalState::Failed {
                reason: if signal == 9 {
                    "process ended by signal 9 (SIGKILL); out-of-memory termination is possible"
                        .into()
                } else {
                    format!("process ended by signal {signal}")
                },
            }
        } else {
            TaskTerminalState::Failed {
                reason: "process ended without an exit status".into(),
            }
        };
        let Some(output) = entry.output.as_ref() else {
            tracing::warn!(%session_id, task = %task, "running task lost its live output buffer");
            return;
        };
        let (output_bytes, truncated, tail, retained) = {
            let buffer = lock_task_output(output);
            (
                buffer.total_bytes(),
                buffer.truncated(),
                buffer.tail_lossy(),
                buffer.retained().to_vec(),
            )
        };
        let (artifact, full_output_unavailable) = if retained.is_empty() {
            (None, false)
        } else {
            match self.hub.put_internal_artifact(retained).await {
                Ok(artifact) => (Some(artifact), false),
                Err(error) => {
                    tracing::warn!(%session_id, task = %task, ?error, "task output artifact was not stored");
                    (None, true)
                }
            }
        };
        let mut completed = TaskCompleted {
            task: task.clone(),
            name: entry.name.clone(),
            state: state.clone(),
            elapsed_ms: now_ms().saturating_sub(entry.started_at_ms),
            output_bytes,
            tail,
            artifact,
            truncated,
            full_output_unavailable,
            delivery: TaskCompletionDelivery::DeliveredQueued,
            workspace_mutation: status.workspace_mutation.clone(),
        };
        if completed.artifact.is_some() {
            // Publish the CAS reference and discard the registry and
            // completion-local live buffer handles immediately after the
            // durable write succeeds.
            // The provisional fact also preserves summary metadata while
            // delivery and journaling finish; `set_terminal` replaces it
            // with the final delivery disposition below.
            registry.stage_durable_output(session_id, task, &completed);
            entry.output = None;
        }
        let notice =
            haider_core::task_event_notice(&TaskEventPayload::TaskCompleted(completed.clone()));
        let delivery = match self.active_run(session_id).await {
            Ok(Some((active_run, active_branch))) => {
                match self
                    .steer_completion(session_id, &active_run, active_branch, &entry, &notice)
                    .await
                {
                    Ok(true) => TaskCompletionDelivery::DeliveredSteer,
                    Ok(false) => TaskCompletionDelivery::DeliveredQueued,
                    Err(error) => {
                        tracing::warn!(%session_id, task = %task, ?error, "task completion steer fell back to queued delivery");
                        TaskCompletionDelivery::DeliveredQueued
                    }
                }
            }
            Ok(None) => TaskCompletionDelivery::DeliveredQueued,
            Err(error) => {
                tracing::warn!(%session_id, task = %task, ?error, "active-run scan failed; task completion delivers queued");
                TaskCompletionDelivery::DeliveredQueued
            }
        };
        completed.delivery = delivery;
        // Exactly ONE prompt copy: the durable steer user message owns it on
        // the steer path, the fact owns it on the queued path.
        let prompt = match delivery {
            TaskCompletionDelivery::DeliveredSteer => PromptRender::Omit,
            TaskCompletionDelivery::DeliveredQueued => PromptRender::Verbatim,
        };
        match completed.to_payload_value() {
            Ok(payload) => {
                let mut envelopes = [self.task_fact_envelope(
                    session_id,
                    &entry.run_id,
                    entry.branch_id.as_ref(),
                    entry.agent_id.as_ref(),
                    &format!("task-completed-{}", task.as_str()),
                    payload,
                    prompt,
                )];
                if let Err(error) = self.hub.append(&mut envelopes).await {
                    tracing::warn!(%session_id, task = %task, ?error, "task completion fact was not journaled");
                }
            }
            Err(error) => {
                tracing::warn!(%session_id, task = %task, ?error, "task completion fact was not encodable");
            }
        }
        registry.set_terminal(session_id, task, state, completed);
    }

    /// Bounded tail/cursor read (LT5): no broker — reading output is not an
    /// effect (the `request_input`/`message_subagent` actor-owned pattern).
    pub(crate) async fn task_output(
        &self,
        session_id: &SessionId,
        task_id: &str,
        cursor: Option<u64>,
    ) -> ToolResult<BoundedResult> {
        self.adopt_session(session_id)
            .await
            .map_err(runtime_tool_error)?;
        let task = TaskId::new(task_id);
        let Some(entry) = self.hub.task_registry().get(session_id, &task) else {
            return Ok(unknown_task_result(task_id));
        };
        let terminal_fact = entry.terminal_fact.clone();
        let artifact = match &entry.state {
            TaskLiveState::Running => None,
            TaskLiveState::Terminal(_) => terminal_fact
                .as_ref()
                .and_then(|fact| fact.artifact.clone()),
        };
        let state = task_state_value(&entry.state);
        let (preview, result_cursor, truncated) = match cursor {
            None => {
                let (output_bytes, truncated, tail) = if let Some(fact) = terminal_fact.as_ref() {
                    (fact.output_bytes, fact.truncated, fact.tail.clone())
                } else {
                    let Some(output) = entry.output.as_ref() else {
                        return Err(missing_task_output_backing(&task));
                    };
                    let buffer = lock_task_output(output);
                    (
                        buffer.total_bytes(),
                        buffer.truncated(),
                        buffer.tail_lossy(),
                    )
                };
                (
                    json!({
                        "task_id": task,
                        "name": entry.name,
                        "state": state,
                        "output_bytes": output_bytes,
                        "truncated": truncated,
                        "tail": tail,
                    }),
                    None,
                    truncated,
                )
            }
            Some(cursor) => {
                let (bytes, next_cursor, exhausted, output_bytes, truncated) =
                    if let Some((artifact, output_bytes, truncated)) =
                        terminal_fact.as_ref().and_then(|fact| {
                            fact.artifact
                                .as_ref()
                                .map(|artifact| (artifact, fact.output_bytes, fact.truncated))
                        })
                    {
                        let retained = self
                            .hub
                            .get_internal_artifact(artifact)
                            .await
                            .map_err(|error| ToolError::cas(error.message))?;
                        let (bytes, next_cursor, exhausted) =
                            read_task_output_page(&retained, cursor, TASK_OUTPUT_READ_BYTES);
                        (bytes, next_cursor, exhausted, output_bytes, truncated)
                    } else {
                        let Some(output) = entry.output.as_ref() else {
                            return Err(missing_task_output_backing(&task));
                        };
                        let buffer = lock_task_output(output);
                        let (bytes, next_cursor, exhausted) = read_task_output_page(
                            buffer.retained(),
                            cursor,
                            TASK_OUTPUT_READ_BYTES,
                        );
                        (
                            bytes,
                            next_cursor,
                            exhausted,
                            buffer.total_bytes(),
                            buffer.truncated(),
                        )
                    };
                (
                    json!({
                        "task_id": task,
                        "name": entry.name,
                        "state": state,
                        "output_bytes": output_bytes,
                        "truncated": truncated,
                        "chunk": String::from_utf8_lossy(&bytes).into_owned(),
                        "next_cursor": next_cursor,
                        "exhausted": exhausted,
                    }),
                    Some(next_cursor.to_string()),
                    truncated,
                )
            }
        };
        Ok(BoundedResult {
            preview: preview.to_string(),
            truncated,
            data: None,
            artifact,
            images: Vec::new(),
            cursor: result_cursor,
            status: ToolResultStatus::Completed,
            reason: None,
            presentation: None,
        })
    }

    /// Brokered pgid kill (LT5): journals intent/outcome around the
    /// supervised TERM → grace → KILL ladder and waits for the terminal
    /// settle so `Ok` means the group actually died.
    pub(crate) async fn task_kill(
        &self,
        session_id: &SessionId,
        task_id: &str,
        broker: &mut EffectBroker,
        policy: &PermissionPolicy,
    ) -> ToolResult<BoundedResult> {
        self.adopt_session(session_id)
            .await
            .map_err(runtime_tool_error)?;
        let task = TaskId::new(task_id);
        let Some(entry) = self.hub.task_registry().get(session_id, &task) else {
            return Ok(unknown_task_result(task_id));
        };
        if let TaskLiveState::Terminal(state) = &entry.state {
            return Ok(BoundedResult {
                preview: json!({
                    "task_id": task,
                    "status": "already_terminal",
                    "state": task_state_value(&TaskLiveState::Terminal(state.clone())),
                })
                .to_string(),
                truncated: false,
                data: None,
                artifact: None,
                images: Vec::new(),
                cursor: None,
                status: ToolResultStatus::Completed,
                reason: None,
                presentation: None,
            });
        }
        let operation = TaskKillEffect {
            task: task.clone(),
            name: entry.name.clone(),
            pid: entry.pid,
        };
        let intent = broker.begin_task_effect(&operation, policy).await?;
        let result = self.kill_and_settle(session_id, &task, &entry).await;
        let state = broker.finish_task_effect(&intent, result).await?;
        Ok(BoundedResult {
            preview: json!({
                "task_id": task,
                "status": "killed",
                "state": task_state_value(&TaskLiveState::Terminal(state)),
            })
            .to_string(),
            truncated: false,
            data: None,
            artifact: None,
            images: Vec::new(),
            cursor: None,
            status: ToolResultStatus::Completed,
            reason: None,
            presentation: None,
        })
    }

    async fn kill_and_settle(
        &self,
        session_id: &SessionId,
        task: &TaskId,
        entry: &TaskEntry,
    ) -> ToolResult<TaskTerminalState> {
        let Some(kill) = entry.kill.as_ref() else {
            return Err(ToolError::Runtime {
                message: format!("background task `{task}` has no live supervision to kill"),
            });
        };
        kill.kill();
        let deadline = tokio::time::Instant::now() + self.kill_grace * 2 + KILL_SETTLE_MARGIN;
        loop {
            if let Some(TaskEntry {
                state: TaskLiveState::Terminal(state),
                ..
            }) = self.hub.task_registry().get(session_id, task)
            {
                return Ok(state);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(ToolError::Runtime {
                    message: format!(
                        "background task `{task}` did not settle after the kill ladder"
                    ),
                });
            }
            tokio::time::sleep(KILL_SETTLE_POLL).await;
        }
    }

    /// Latest run a steer can actually reach: nonterminal, not cancelling,
    /// and not merely Queued — a queued run has no live harness to nudge,
    /// and its prompt is compiled AFTER the completion fact lands, so the
    /// Verbatim fact already reaches it (queued delivery, honestly).
    async fn active_run(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<(RunId, Option<BranchId>)>, HaiderError> {
        let mut cursor = 0;
        let mut states = HashMap::<RunId, (RunState, Option<BranchId>, u64)>::new();
        loop {
            let page = self
                .hub
                .read_internal_session(session_id, cursor, 256)
                .await?;
            if page.is_empty() {
                break;
            }
            cursor = page.last().map_or(cursor, |envelope| envelope.seq);
            for envelope in page {
                let Some(run_id) = envelope.run_id.clone() else {
                    continue;
                };
                if let Ok(haider_protocol::EventPayload::RunState(state)) =
                    serde_json::from_value::<haider_protocol::EventPayload>(envelope.payload)
                {
                    states.insert(run_id, (state, envelope.branch_id, envelope.seq));
                }
            }
        }
        Ok(states
            .into_iter()
            .filter(|(_, (state, _, _))| {
                !state.is_terminal() && *state != RunState::Cancelling && *state != RunState::Queued
            })
            .max_by_key(|(_, (_, _, seq))| *seq)
            .map(|(run_id, (_, branch_id, _))| (run_id, branch_id)))
    }

    /// Durable STEER into the active run + best-effort live wake — exactly
    /// the S1 delivered_steer seam. A disposition race (the run ended
    /// between the scan and the accept) cancels the stray admission and
    /// falls back to queued delivery.
    async fn steer_completion(
        &self,
        session_id: &SessionId,
        run_id: &RunId,
        branch_id: Option<BranchId>,
        entry: &TaskEntry,
        notice: &str,
    ) -> Result<bool, HaiderError> {
        let identity = format!("{}-{}", entry.task.as_str(), run_id.as_str());
        let request_json = serde_json::to_string(&json!({
            "session_id": session_id,
            "run_id": run_id,
            "task": entry.task,
            "text": notice,
            "mode": DeliveryMode::Steer,
        }))
        .map_err(internal_serialization)?;
        let accepted = self
            .hub
            .accept_internal_turn(TurnAcceptCommand {
                command_id: format!("task-steer-{identity}"),
                request_digest: crate::delegation::digest_bytes(request_json.as_bytes()),
                request_json,
                session_id: session_id.clone(),
                worker_generation: self.hub.worker_generation(),
                branch_id,
                run_id: run_id.clone(),
                agent_id: entry.agent_id.clone(),
                text: notice.to_owned(),
                attachments: Vec::new(),
                mode: DeliveryMode::Steer,
                queued_event_id: EventId::new(format!("task-steer-queued-{identity}")),
                user_event_id: EventId::new(format!("task-steer-user-{identity}")),
                active_event_id: EventId::new(format!("task-steer-active-{identity}")),
                device_id: self.hub.device_id(),
            })
            .await?;
        if accepted.disposition != TurnAdmissionDisposition::SteerPending {
            // The run went terminal in the race window and the admission
            // became a fresh queued turn: cancel it — a task completion must
            // never START provider work on its own.
            let request_json = serde_json::to_string(&json!({
                "session_id": session_id,
                "run_id": accepted.run_id,
                "reason": "task-steer-race",
            }))
            .map_err(internal_serialization)?;
            let cancel = TurnCancelCommand {
                command_id: format!("task-steer-race-cancel-{identity}"),
                request_digest: crate::delegation::digest_bytes(request_json.as_bytes()),
                request_json,
                session_id: session_id.clone(),
                worker_generation: self.hub.worker_generation(),
                run_id: accepted.run_id.clone(),
                cancelling_event_id: EventId::new(format!("task-steer-race-cancelling-{identity}")),
                device_id: self.hub.device_id(),
            };
            if let Err(error) = self.hub.cancel_internal_turn(cancel).await {
                tracing::warn!(%session_id, ?error, "stray task-steer admission was not cancelled");
            }
            return Ok(false);
        }
        // The durable steer IS the delivery (the S1 law): a restarted turn
        // recompiles its prompt with this committed steer message, so a
        // failed live wake never demotes the delivery to queued — that
        // would journal a SECOND prompt copy through the Verbatim fact.
        if let Err(error) = self
            .hub
            .submit_internal_nudge(accepted, notice.to_owned())
            .await
        {
            tracing::warn!(%session_id, ?error, "durable task-completion steer wake was not delivered");
        }
        Ok(true)
    }

    async fn scan_session_tasks(&self, session_id: &SessionId) -> Result<TaskScan, HaiderError> {
        let mut cursor = 0;
        let mut scan = TaskScan::default();
        loop {
            let page = self
                .hub
                .read_internal_session(session_id, cursor, 256)
                .await?;
            if page.is_empty() {
                return Ok(scan);
            }
            cursor = page.last().map_or(cursor, |envelope| envelope.seq);
            for envelope in page {
                let Some(event) = TaskEventPayload::from_payload_value(&envelope.payload) else {
                    continue;
                };
                match event {
                    TaskEventPayload::TaskStarted(fact) => {
                        scan.started.insert(
                            fact.task.clone(),
                            ScannedStart {
                                fact,
                                run_id: envelope
                                    .run_id
                                    .unwrap_or_else(|| RunId::new("task-unknown-run")),
                                branch_id: envelope.branch_id,
                                agent_id: envelope.agent_id,
                            },
                        );
                    }
                    TaskEventPayload::TaskCompleted(fact) => {
                        scan.completed.insert(fact.task.clone(), fact);
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn task_fact_envelope(
        &self,
        session_id: &SessionId,
        run_id: &RunId,
        branch_id: Option<&BranchId>,
        agent_id: Option<&AgentId>,
        event_id: &str,
        payload: serde_json::Value,
        prompt: PromptRender,
    ) -> RawEnvelope {
        EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: EventId::new(event_id),
            seq: 0,
            session_id: session_id.clone(),
            branch_id: branch_id.cloned(),
            run_id: Some(run_id.clone()),
            agent_id: agent_id.cloned(),
            device_id: self.hub.device_id(),
            authority_epoch: 0,
            worker_generation: self.hub.worker_generation(),
            causation_id: None,
            correlation_id: None,
            committed_at_ms: 0,
            render: RenderTargets {
                ui: true,
                durable: true,
                prompt,
            },
            payload,
        }
    }
}

/// Test-only fact builder: the exact production envelope shape without a
/// spawned child, for staging prior-life journal states.
#[cfg(all(test, unix))]
pub(crate) fn test_task_fact_envelope(
    hub: &SessionHub,
    session_id: &SessionId,
    run_id: &RunId,
    event_id: &str,
    payload: serde_json::Value,
) -> RawEnvelope {
    TaskFacade::new(hub.clone()).task_fact_envelope(
        session_id,
        run_id,
        None,
        None,
        event_id,
        payload,
        PromptRender::Verbatim,
    )
}

#[derive(Default)]
struct TaskScan {
    started: HashMap<TaskId, ScannedStart>,
    completed: HashMap<TaskId, TaskCompleted>,
}

struct ScannedStart {
    fact: TaskStarted,
    run_id: RunId,
    branch_id: Option<BranchId>,
    agent_id: Option<AgentId>,
}

impl SessionHub {
    /// Session-delete fence (LT7): kill every live pgid this session owns —
    /// registry entries via their supervised ladders (their completion
    /// pipelines find the projection gone and record nothing into the
    /// deleted journal), plus prior-life journal orphans via detached reaps.
    pub(crate) async fn fence_background_tasks(&self, session_id: &SessionId) {
        let entries = self.task_registry().remove_session(session_id);
        let mut fenced_pids = Vec::new();
        for entry in entries {
            fenced_pids.push(entry.pid);
            if entry.state == TaskLiveState::Running
                && let Some(kill) = entry.kill.as_ref()
            {
                kill.kill();
            }
        }
        // Prior-life orphans exist only in the journal (about to be
        // deleted): read the store directly — the session actor is already
        // stopped at this point in the delete path.
        let mut cursor = 0;
        let mut started = HashMap::<TaskId, i32>::new();
        let mut completed = std::collections::HashSet::<TaskId>::new();
        loop {
            let Ok(page) = self.store_read(session_id, cursor, 256).await else {
                break;
            };
            if page.is_empty() {
                break;
            }
            cursor = page.last().map_or(cursor, |envelope| envelope.seq);
            for envelope in page {
                match TaskEventPayload::from_payload_value(&envelope.payload) {
                    Some(TaskEventPayload::TaskStarted(fact)) => {
                        started.insert(fact.task, fact.pid);
                    }
                    Some(TaskEventPayload::TaskCompleted(fact)) => {
                        completed.insert(fact.task);
                    }
                    None => {}
                }
            }
        }
        for (task, pid) in started {
            if completed.contains(&task) || fenced_pids.contains(&pid) {
                continue;
            }
            tokio::spawn(reap_orphan_group(
                pid,
                BACKGROUND_KILL_GRACE,
                probe_group_liveness,
            ));
        }
    }

    /// Daemon-shutdown fence (LT7): every running task's pgid dies with the
    /// daemon. Kills are requested through the supervised ladders and given
    /// a bounded settle so completion facts can still journal before the
    /// hub drains.
    pub(crate) async fn shutdown_background_tasks(&self) {
        let registry = self.task_registry();
        let sessions = registry.sessions_with_running();
        if sessions.is_empty() {
            return;
        }
        for session_id in &sessions {
            for entry in registry.running_entries(session_id) {
                if let Some(kill) = entry.kill.as_ref() {
                    kill.kill();
                }
            }
        }
        let deadline = tokio::time::Instant::now() + BACKGROUND_KILL_GRACE + KILL_SETTLE_MARGIN;
        while tokio::time::Instant::now() < deadline {
            if sessions
                .iter()
                .all(|session_id| registry.running_count(session_id) == 0)
            {
                return;
            }
            tokio::time::sleep(KILL_SETTLE_POLL).await;
        }
        tracing::warn!("background tasks did not settle before shutdown; next start reaps orphans");
    }
}

fn task_state_value(state: &TaskLiveState) -> serde_json::Value {
    match state {
        TaskLiveState::Running => json!("running"),
        TaskLiveState::Terminal(terminal) => {
            serde_json::to_value(terminal).unwrap_or_else(|_| json!("terminal"))
        }
    }
}

fn unknown_task_result(task_id: &str) -> BoundedResult {
    BoundedResult {
        preview: json!({
            "status": "unknown_task",
            "task_id": task_id,
            "message": "no background task with this id exists in this session",
        })
        .to_string(),
        truncated: false,
        data: None,
        artifact: None,
        images: Vec::new(),
        cursor: None,
        status: ToolResultStatus::Unknown,
        reason: Some("no background task with this id exists in this session".into()),
        presentation: None,
    }
}

fn bounded_chars(text: &str, cap: usize) -> String {
    text.chars().take(cap).collect()
}

fn missing_task_output_backing(task: &TaskId) -> ToolError {
    ToolError::Lifecycle {
        message: format!("background task `{task}` has no readable output backing"),
    }
}

fn read_task_output_page(retained: &[u8], cursor: u64, max: usize) -> (Vec<u8>, u64, bool) {
    let retained_len = u64::try_from(retained.len()).unwrap_or(u64::MAX);
    let start = usize::try_from(cursor.min(retained_len)).unwrap_or(usize::MAX);
    let start = start.min(retained.len());
    let end = start.saturating_add(max).min(retained.len());
    (
        retained[start..end].to_vec(),
        u64::try_from(end).unwrap_or(u64::MAX),
        end >= retained.len(),
    )
}

fn now_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

fn runtime_tool_error(error: HaiderError) -> ToolError {
    ToolError::Runtime {
        message: error.message,
    }
}

fn internal_serialization(error: serde_json::Error) -> HaiderError {
    HaiderError::new(
        haider_protocol::error::ErrorCode::Internal,
        format!("cannot encode background task fact: {error}"),
        false,
    )
}

#[cfg(test)]
#[path = "tasks_eviction_tests.rs"]
mod eviction_tests;
