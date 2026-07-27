//! CHARTER — the turn engine: owned per-session supervisors and injectable
//! turn dependencies (report R1).
//!
//! What lives here: [`WorkerManager`] (one lazy supervisor per session, all
//! tasks owned — nothing detached), the supervisor loop (accepted-turn
//! queue, active cancellation, provider/tool/prompt assembly, drain
//! settlement), the turn-scoped [`ProviderFactory`]/[`TurnToolFactory`]
//! ports, and the production broker-backed tool dispatcher with its
//! hub-owned journal/CAS adapters. What may NOT live here: SQLite (a worker
//! holds only its lease-fenced `HubStoreHandle`; a source-scan regression
//! test enforces the module-side half of the R1 append-exclusivity seal),
//! wire/RPC concerns
//! (rpc.rs hands this module a COMMITTED [`AcceptedTurn`], never a raw
//! request), and session-hub actor work (the hub actor must stay free of
//! provider/tool awaits; everything slow happens in supervisor tasks).
//!
//! ADMISSION DISCIPLINE (authoritative statement): a supervisor starts
//! provider work only from durable facts — a submit reaches it after the
//! acceptance transaction committed, and `admit_pending`/`refill_queued_turns`
//! re-derive runnability from journal run states, never from the in-memory
//! message that delivered the hint. The bounded queue may drop hints
//! (`rescan_needed`); the durable `Queued`+`UserMessage` pair is the overflow
//! buffer.

use crate::session_hub::{HubStoreHandle, SessionHub, SessionHubError};
use crate::turn_recovery::{cancelled_resumption_payloads, failed_resumption_payloads};
use async_trait::async_trait;
use base64::Engine;
use haider_core::{
    AcceptedTurn, CancelToken, EventIdGenerator, HarnessActor, HarnessConfig,
    PromptHistoryCompiler, RequestInputCheckpoint, StoreHandle, SubmitCheckpointTurn,
    SubmitCommittedTurn, ToolDispatcher, TurnHandle, sanitized_failure_message,
};
use haider_protocol::EventPayload;
use haider_protocol::effect::{EffectClass, EffectOutcome, EffectPhase};
use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::ids::{DeviceId, EffectId, EventId, RunId, SessionId};
use haider_protocol::session::SessionMetadataV1;
use haider_protocol::state::{RunState, SessionState};
use haider_protocol::tool::BoundedResult;
use haider_provider::{Message, ResolvedAttachment};
use haider_provider::{Provider, ToolDefinition};
use haider_tools::{
    CasSink, EffectBroker, FsList, FsRead, FsSearch, JournalSink, PermissionPolicy, ResultBounds,
    ToolResult,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};

const MANAGER_CAPACITY: usize = 128;
const SUPERVISOR_CAPACITY: usize = 64;

/// A provider resolved and pinned for one logical turn (R6).
pub struct ResolvedTurnProvider {
    pub provider: Arc<dyn Provider>,
    pub provider_name: String,
    pub model: String,
    /// Stamped into every usage snapshot the turn commits; an account change
    /// inside one logical turn is a protocol error in core.
    pub account_alias: Option<String>,
}

/// Injectable, turn-scoped provider resolver (R6, authoritative pinning
/// site): resolution happens once per logical turn, after durable acceptance
/// and before `Thinking`/provider work, and the result is pinned across
/// every provider request in that turn — a login or account switch affects
/// the NEXT logical turn only. `resolve_for_turn` must return the same
/// provider name the session's metadata records; `start_turn` rejects a
/// mismatch.
#[async_trait]
pub trait ProviderFactory: Send + Sync {
    async fn resolve_for_turn(
        &self,
        metadata: &SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, HaiderError>;
}

/// Inputs available to a turn-scoped tool dispatcher factory.
#[derive(Clone)]
pub struct WorkerToolContext {
    pub metadata: SessionMetadataV1,
    pub store: HubStoreHandle,
    pub run_id: RunId,
    pub device_id: DeviceId,
    pub event_ids: Arc<EventIdGenerator>,
}

/// Injectable tool/effect boundary (R4). Production uses the shipped broker;
/// tests can hold a dispatch at an exact crash boundary.
///
/// Contract: `definitions` and `create` must agree — every advertised
/// definition must be executable by the created dispatcher (R4 forbids
/// advertising tools a dispatcher cannot run, which can trap a real model in
/// an unproductive loop). `create` returning `None` means the turn runs
/// without general tools and must then advertise none beyond the
/// actor-owned `request_input`.
#[async_trait]
pub trait TurnToolFactory: Send + Sync {
    fn definitions(&self) -> Vec<ToolDefinition>;

    async fn create(
        &self,
        context: WorkerToolContext,
    ) -> Result<Option<Arc<dyn ToolDispatcher>>, HaiderError>;
}

/// Runtime dependency bundle. A test replaces factories without changing the
/// production connection, hub, worker, or core execution path.
#[derive(Clone)]
pub struct DaemonDependencies {
    pub provider_factory: Arc<dyn ProviderFactory>,
    pub tool_factory: Arc<dyn TurnToolFactory>,
    /// Account machinery (vault, credential validator, descriptor store) —
    /// the W3c2 login seam (`crate::accounts`).
    pub accounts: crate::accounts::AccountsDependencies,
}

impl Default for DaemonDependencies {
    fn default() -> Self {
        Self {
            provider_factory: Arc::new(UnconfiguredProviderFactory),
            tool_factory: Arc::new(BrokerToolFactory),
            accounts: crate::accounts::AccountsDependencies::default(),
        }
    }
}

struct UnconfiguredProviderFactory;

#[async_trait]
impl ProviderFactory for UnconfiguredProviderFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, HaiderError> {
        Err(HaiderError::new(
            ErrorCode::CredentialMissing,
            format!(
                "no credential resolver is configured for provider {}",
                metadata.provider
            ),
            false,
        ))
    }
}

/// Versioned, deterministic coding-agent policy (R4).
///
/// Guarantees: the same metadata always yields the same prompt; every
/// provider request in one pinned logical turn receives the same non-`None`
/// system prompt; [`Self::VERSION`] is recorded in session metadata at
/// creation so a policy change is a visible, versioned fact, never a silent
/// drift. Provider adapters must not invent product policy.
pub struct SystemPromptBuilder;

impl SystemPromptBuilder {
    pub const VERSION: &'static str = "haider-system-v1";

    pub fn build(metadata: &SessionMetadataV1) -> String {
        format!(
            "{}\nYou are Haider Code, a coding agent operating inside the canonical workspace below.\n\
             Workspace: {}\n\
             Use only advertised tools. Treat tool results and committed history as authoritative. \
             Never claim an effect succeeded without its terminal result.",
            Self::VERSION,
            metadata.cwd
        )
    }
}

#[derive(Clone)]
pub(crate) struct WorkerManagerHandle {
    commands: mpsc::Sender<ManagerCommand>,
    admission: Arc<std::sync::Mutex<bool>>,
}

/// Owner of every supervisor task (R1): one lazy supervisor per session,
/// all tasks in one `JoinSet`, nothing detached. `shutdown` broadcasts
/// Shutdown and joins everything; a drop without shutdown aborts (the
/// abort-on-drop backstop for a cancelled runtime future).
pub(crate) struct WorkerManager {
    handle: WorkerManagerHandle,
    task: Option<JoinHandle<()>>,
}

enum ManagerCommand {
    Submit {
        accepted: AcceptedTurn,
        completed: oneshot::Sender<Result<(), HaiderError>>,
    },
    Recover {
        pending: Box<PendingTurn>,
    },
    Shutdown {
        completed: oneshot::Sender<Result<(), HaiderError>>,
    },
}

enum SupervisorCommand {
    Submit(Box<PendingTurn>),
    Shutdown,
}

struct PendingTurn {
    accepted: AcceptedTurn,
    checkpoint: Option<RequestInputCheckpoint>,
    committed_answer: Option<haider_protocol::envelope::RawEnvelope>,
    recovery_ready: Option<oneshot::Sender<Result<(), HaiderError>>>,
    /// Recovery semantics outlive the pre-Ready acknowledgement. In
    /// particular, a queued recovery may be acknowledged behind a parked
    /// checkpoint but must still use recovery-shaped closure if its eventual
    /// provider/credential resolution fails.
    recovering: bool,
}

struct SupervisorSlot {
    sender: mpsc::Sender<SupervisorCommand>,
    task_id: tokio::task::Id,
}

struct SupervisorExit {
    session_id: SessionId,
    terminalize_nonterminal: bool,
}

impl PendingTurn {
    fn accepted(accepted: AcceptedTurn) -> Self {
        Self {
            accepted,
            checkpoint: None,
            committed_answer: None,
            recovery_ready: None,
            recovering: false,
        }
    }
}

impl WorkerManager {
    pub(crate) fn start(hub: SessionHub, dependencies: DaemonDependencies) -> Self {
        let (commands, receiver) = mpsc::channel(MANAGER_CAPACITY);
        let handle = WorkerManagerHandle {
            commands,
            admission: Arc::new(std::sync::Mutex::new(true)),
        };
        let task = tokio::spawn(run_manager(hub, dependencies, receiver));
        Self {
            handle,
            task: Some(task),
        }
    }

    pub(crate) fn handle(&self) -> WorkerManagerHandle {
        self.handle.clone()
    }

    pub(crate) async fn shutdown(mut self) -> Result<(), HaiderError> {
        self.handle.begin_draining();
        let (completed, response) = oneshot::channel();
        self.handle
            .commands
            .send(ManagerCommand::Shutdown { completed })
            .await
            .map_err(|_| manager_stopped())?;
        let result = response.await.map_err(|_| manager_stopped())?;
        if let Some(task) = self.task.take() {
            task.await.map_err(|error| {
                HaiderError::new(
                    ErrorCode::Internal,
                    format!("worker manager shutdown failed: {error}"),
                    true,
                )
            })?;
        }
        result
    }

    /// Abrupt owner teardown for the in-process process-death seam.
    ///
    /// Unlike `shutdown`, this sends no cancellation command and appends no
    /// terminal event. Startup recovery must decide what the durable prefix
    /// means. This is intentionally distinct from a child-supervisor panic:
    /// the live manager observes those through its JoinSet, terminalizes the
    /// run, evicts the slot, and retains/increments the session incarnation
    /// before recreation. Eviction and incarnation are inseparable because a
    /// same-generation EventIdGenerator namespace must never be reused.
    pub(crate) async fn crash(mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for WorkerManager {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

impl WorkerManagerHandle {
    pub(crate) fn begin_draining(&self) {
        if let Ok(mut open) = self.admission.lock() {
            *open = false;
        }
    }

    pub(crate) async fn submit(&self, accepted: AcceptedTurn) -> Result<(), HaiderError> {
        let (completed, response) = oneshot::channel();
        {
            let open = self.admission.lock().map_err(|_| manager_stopped())?;
            if !*open {
                return Err(manager_busy("worker admission is draining"));
            }
            self.commands
                .try_send(ManagerCommand::Submit {
                    accepted,
                    completed,
                })
                .map_err(manager_try_send)?;
        }
        response.await.map_err(|_| manager_stopped())?
    }

    fn send_recovery(&self, pending: PendingTurn) -> Result<(), HaiderError> {
        self.commands
            .try_send(ManagerCommand::Recover {
                pending: Box::new(pending),
            })
            .map_err(manager_try_send)
    }

    pub(crate) async fn recover_checkpoint(
        &self,
        accepted: AcceptedTurn,
        checkpoint: RequestInputCheckpoint,
        committed_answer: Option<haider_protocol::envelope::RawEnvelope>,
    ) -> Result<(), HaiderError> {
        let (completed, response) = oneshot::channel();
        self.send_recovery(PendingTurn {
            accepted,
            checkpoint: Some(checkpoint),
            committed_answer,
            recovery_ready: Some(completed),
            recovering: true,
        })?;
        response.await.map_err(|_| manager_stopped())?
    }

    pub(crate) async fn recover_queued(&self, accepted: AcceptedTurn) -> Result<(), HaiderError> {
        let (completed, response) = oneshot::channel();
        self.send_recovery(PendingTurn {
            accepted,
            checkpoint: None,
            committed_answer: None,
            recovery_ready: Some(completed),
            recovering: true,
        })?;
        response.await.map_err(|_| manager_stopped())?
    }
}

async fn run_manager(
    hub: SessionHub,
    dependencies: DaemonDependencies,
    mut commands: mpsc::Receiver<ManagerCommand>,
) {
    let mut supervisors = HashMap::<SessionId, SupervisorSlot>::new();
    let mut incarnations = HashMap::<SessionId, u64>::new();
    let mut task_sessions = HashMap::<tokio::task::Id, SessionId>::new();
    let mut tasks = JoinSet::<SupervisorExit>::new();
    loop {
        let command = tokio::select! {
            biased;
            outcome = tasks.join_next_with_id(), if !tasks.is_empty() => {
                if let Some(outcome) = outcome {
                    handle_supervisor_exit(
                        &hub,
                        &mut supervisors,
                        &mut task_sessions,
                        &mut incarnations,
                        outcome,
                    ).await;
                }
                continue;
            }
            command = commands.recv() => command,
        };
        let Some(command) = command else {
            break;
        };
        match command {
            ManagerCommand::Submit {
                accepted,
                completed,
            } => {
                let result = match supervisor_for(
                    &hub,
                    &dependencies,
                    &mut supervisors,
                    &mut tasks,
                    &mut task_sessions,
                    &mut incarnations,
                    accepted.session_id.clone(),
                )
                .await
                {
                    Ok(supervisor) => supervisor
                        .try_send(SupervisorCommand::Submit(Box::new(PendingTurn::accepted(
                            accepted,
                        ))))
                        .map_err(supervisor_try_send),
                    Err(error) => Err(error),
                };
                let _ = completed.send(result);
            }
            ManagerCommand::Recover { mut pending } => {
                let session_id = pending.accepted.session_id.clone();
                match supervisor_for(
                    &hub,
                    &dependencies,
                    &mut supervisors,
                    &mut tasks,
                    &mut task_sessions,
                    &mut incarnations,
                    session_id,
                )
                .await
                {
                    Ok(supervisor) => {
                        if let Err(error) = supervisor.try_send(SupervisorCommand::Submit(pending))
                        {
                            let (mut pending, error) = match error {
                                mpsc::error::TrySendError::Full(SupervisorCommand::Submit(
                                    pending,
                                )) => (pending, manager_busy("recovered work queue is full")),
                                mpsc::error::TrySendError::Closed(SupervisorCommand::Submit(
                                    pending,
                                )) => (pending, manager_stopped()),
                                _ => unreachable!(),
                            };
                            if let Some(ready) = pending.recovery_ready.take() {
                                let result =
                                    terminalize_recovery_feed_failure(&hub, *pending, error).await;
                                let _ = ready.send(result);
                            }
                        }
                    }
                    Err(error) => {
                        if let Some(ready) = pending.recovery_ready.take() {
                            let result =
                                terminalize_recovery_feed_failure(&hub, *pending, error).await;
                            let _ = ready.send(result);
                        }
                    }
                }
            }
            ManagerCommand::Shutdown { completed } => {
                for supervisor in supervisors.values() {
                    let _ = supervisor.sender.send(SupervisorCommand::Shutdown).await;
                }
                while let Some(outcome) = tasks.join_next_with_id().await {
                    handle_supervisor_exit(
                        &hub,
                        &mut supervisors,
                        &mut task_sessions,
                        &mut incarnations,
                        outcome,
                    )
                    .await;
                }
                let result = drain_accepted_without_handoff(&hub).await;
                let _ = completed.send(result);
                return;
            }
        }
    }
    for supervisor in supervisors.values() {
        let _ = supervisor.sender.send(SupervisorCommand::Shutdown).await;
    }
    while tasks.join_next().await.is_some() {}
}

async fn terminalize_recovery_feed_failure(
    hub: &SessionHub,
    pending: PendingTurn,
    error: HaiderError,
) -> Result<(), HaiderError> {
    let run_id = pending.accepted.run_id;
    let session_id = pending.accepted.session_id;
    let lease = hub
        .acquire_worker_lease(session_id.clone())
        .await
        .map_err(hub_error)?;
    let device_id = DeviceId::new(format!(
        "recovery-feed-worker-{}-{}-{}",
        session_id,
        lease.worker_generation(),
        run_id,
    ));
    let event_ids = EventIdGenerator::new(format!(
        "recovery-feed-event-{}-{}-{}",
        session_id,
        lease.worker_generation(),
        run_id,
    ));
    let mut payloads = failed_resumption_payloads(&lease, &session_id, &run_id, &error).await?;
    payloads.retain(|payload| !matches!(payload, EventPayload::SessionState(_)));
    append_payloads(&lease, &device_id, &run_id, &event_ids, payloads).await?;
    append_session_idle(&lease, &device_id, &event_ids, true).await?;
    let _ = lease.unregister_worker().await;
    tracing::warn!(
        %session_id,
        %run_id,
        ?error,
        "recovered work could not enter a supervisor and was terminalized"
    );
    Ok(())
}

async fn drain_accepted_without_handoff(hub: &SessionHub) -> Result<(), HaiderError> {
    let session_ids = hub.session_ids().await.map_err(hub_error)?;
    for session_id in session_ids {
        let lease = match hub
            .acquire_drain_worker_lease(session_id.clone())
            .await
            .map_err(hub_error)?
        {
            Some(lease) => lease,
            None => continue,
        };
        let device_id = DeviceId::new(format!(
            "drain-worker-{}-{}",
            session_id,
            lease.worker_generation()
        ));
        let event_ids = EventIdGenerator::new(format!(
            "drain-event-{}-{}",
            session_id,
            lease.worker_generation()
        ));
        let mut terminalized = false;
        for (run_id, state, _) in durable_runs(&lease).await? {
            if state.is_terminal() {
                continue;
            }
            if state != RunState::Cancelling {
                append_run_state(
                    &lease,
                    &device_id,
                    &run_id,
                    &event_ids,
                    RunState::Cancelling,
                )
                .await?;
            }
            reconcile_unknown_effects(&lease, &device_id, &run_id, &event_ids).await?;
            let mut payloads = cancelled_resumption_payloads(&lease, &session_id, &run_id).await?;
            payloads.retain(|payload| !matches!(payload, EventPayload::SessionState(_)));
            append_payloads(&lease, &device_id, &run_id, &event_ids, payloads).await?;
            terminalized = true;
        }
        if terminalized {
            append_session_idle(&lease, &device_id, &event_ids, true).await?;
        }
        lease.unregister_worker().await.map_err(hub_error)?;
    }
    Ok(())
}

async fn handle_supervisor_exit(
    hub: &SessionHub,
    supervisors: &mut HashMap<SessionId, SupervisorSlot>,
    task_sessions: &mut HashMap<tokio::task::Id, SessionId>,
    incarnations: &mut HashMap<SessionId, u64>,
    outcome: Result<(tokio::task::Id, SupervisorExit), tokio::task::JoinError>,
) {
    let (task_id, session_id, panicked, terminalize_nonterminal) = match outcome {
        Ok((task_id, exit)) => (
            task_id,
            exit.session_id,
            false,
            exit.terminalize_nonterminal,
        ),
        Err(error) => {
            let task_id = error.id();
            let Some(session_id) = task_sessions.get(&task_id).cloned() else {
                tracing::error!(?error, "unknown supervisor task failed");
                return;
            };
            (task_id, session_id, error.is_panic(), true)
        }
    };
    task_sessions.remove(&task_id);
    if supervisors
        .get(&session_id)
        .is_some_and(|slot| slot.task_id == task_id)
    {
        supervisors.remove(&session_id);
    }
    // EVICTION + INCARNATION are one law: eviction makes later submissions
    // usable again, while the next `supervisor_for` increments the retained
    // incarnation before constructing its EventIdGenerator. Never evict
    // without retaining this counter or a recreated supervisor could collide
    // with event IDs minted by its predecessor in the same store generation.
    let incarnation = *incarnations.entry(session_id.clone()).or_insert(1);
    if terminalize_nonterminal
        && let Err(error) = terminalize_supervisor_exit(hub, &session_id, incarnation).await
    {
        tracing::error!(
            %session_id,
            ?error,
            panicked,
            "exited supervisor work could not be terminalized"
        );
    }
}

pub(crate) async fn terminalize_supervisor_exit(
    hub: &SessionHub,
    session_id: &SessionId,
    incarnation: u64,
) -> Result<(), HaiderError> {
    let lease = hub
        .acquire_worker_lease(session_id.clone())
        .await
        .map_err(hub_error)?;
    let device_id = DeviceId::new(format!(
        "panic-worker-{}-{}-{}",
        session_id,
        lease.worker_generation(),
        incarnation,
    ));
    let event_ids = EventIdGenerator::new(format!(
        "panic-event-{}-{}-{}",
        session_id,
        lease.worker_generation(),
        incarnation,
    ));
    let runs = durable_runs(&lease)
        .await?
        .into_iter()
        .filter(|(_, state, _)| !state.is_terminal())
        .collect::<Vec<_>>();
    for (run_id, state, _) in &runs {
        // Panic can strand a dispatched effect regardless of the run state.
        // Reconcile before either cancellation-shaped or failure-shaped
        // terminalization; a reconciliation error fences every terminal.
        reconcile_unknown_effects(&lease, &device_id, run_id, &event_ids).await?;
        if *state == RunState::Cancelling {
            let mut payloads = cancelled_resumption_payloads(&lease, session_id, run_id).await?;
            payloads.retain(|payload| !matches!(payload, EventPayload::SessionState(_)));
            append_payloads(&lease, &device_id, run_id, &event_ids, payloads).await?;
            continue;
        }
        let error = HaiderError::new(
            ErrorCode::Internal,
            "session supervisor exited before the run completed",
            true,
        );
        let mut payloads = failed_resumption_payloads(&lease, session_id, run_id, &error).await?;
        payloads.retain(|payload| !matches!(payload, EventPayload::SessionState(_)));
        append_payloads(&lease, &device_id, run_id, &event_ids, payloads).await?;
    }
    if !runs.is_empty() {
        append_session_idle(&lease, &device_id, &event_ids, true).await?;
    }
    let _ = lease.unregister_worker().await;
    Ok(())
}

async fn supervisor_for(
    hub: &SessionHub,
    dependencies: &DaemonDependencies,
    supervisors: &mut HashMap<SessionId, SupervisorSlot>,
    tasks: &mut JoinSet<SupervisorExit>,
    task_sessions: &mut HashMap<tokio::task::Id, SessionId>,
    incarnations: &mut HashMap<SessionId, u64>,
    session_id: SessionId,
) -> Result<mpsc::Sender<SupervisorCommand>, HaiderError> {
    if let Some(supervisor) = supervisors.get(&session_id) {
        return if supervisor.sender.is_closed() {
            Err(manager_busy(
                "session supervisor is being evicted after exit",
            ))
        } else {
            Ok(supervisor.sender.clone())
        };
    }
    let metadata = hub.session_metadata(&session_id).await?.ok_or_else(|| {
        HaiderError::new(
            ErrorCode::InvalidArgument,
            "legacy session has no live-worker metadata",
            false,
        )
    })?;
    let (cancellation_wake, cancellation_wakes) = tokio::sync::watch::channel(0_u64);
    let lease = hub
        .acquire_worker_lease_with_cancellation_wake(session_id.clone(), cancellation_wake)
        .await
        .map_err(hub_error)?;
    let (sender, receiver) = mpsc::channel(SUPERVISOR_CAPACITY);
    let incarnation = *incarnations
        .entry(session_id.clone())
        .and_modify(|incarnation| *incarnation = incarnation.saturating_add(1))
        .or_insert(1);
    let task_session_id = session_id.clone();
    let supervisor_dependencies = dependencies.clone();
    let task = tasks.spawn(async move {
        let terminalize_nonterminal = run_supervisor(
            supervisor_dependencies,
            metadata,
            lease,
            receiver,
            cancellation_wakes,
            incarnation,
        )
        .await;
        SupervisorExit {
            session_id: task_session_id,
            terminalize_nonterminal,
        }
    });
    let task_id = task.id();
    task_sessions.insert(task_id, session_id.clone());
    supervisors.insert(
        session_id,
        SupervisorSlot {
            sender: sender.clone(),
            task_id,
        },
    );
    Ok(sender)
}

struct ActiveTurn {
    run_id: RunId,
    cancel: CancelToken,
    outcome: Pin<Box<dyn FutureTurn>>,
    harness: haider_core::HarnessHandle,
    dispatcher: Option<Arc<dyn ToolDispatcher>>,
    actor: Option<JoinHandle<()>>,
}

impl Drop for ActiveTurn {
    fn drop(&mut self) {
        if let Some(actor) = &self.actor {
            actor.abort();
        }
    }
}

trait FutureTurn:
    std::future::Future<Output = Result<haider_core::TurnOutcome, HaiderError>> + Send
{
}
impl<T> FutureTurn for T where
    T: std::future::Future<Output = Result<haider_core::TurnOutcome, HaiderError>> + Send
{
}

/// One session's turn loop: strictly serial turns from the bounded queue,
/// with three live inputs while a turn runs — submissions (queued behind the
/// active run), the hub's cancellation wake (durable `Cancelling` reconciled
/// from the journal, active token cancelled), and the active turn's outcome
/// (dispatcher closed, harness stopped and joined, then the store-side
/// conditional Idle settle — `Store::settle_session_idle` owns that law).
/// Shutdown cancels the active turn, terminalizes durable queued runs, and
/// exits only after the last turn settles; the supervisor deregisters its
/// lease on the way out.
async fn run_supervisor(
    dependencies: DaemonDependencies,
    metadata: SessionMetadataV1,
    lease: HubStoreHandle,
    mut commands: mpsc::Receiver<SupervisorCommand>,
    mut cancellation_wakes: tokio::sync::watch::Receiver<u64>,
    incarnation: u64,
) -> bool {
    let mut queue = VecDeque::<PendingTurn>::new();
    let mut active: Option<ActiveTurn> = None;
    let device_id = DeviceId::new(format!(
        "worker-{}-{}-{}",
        lease.session_id(),
        lease.worker_generation(),
        incarnation,
    ));
    let event_ids = Arc::new(EventIdGenerator::new(format!(
        "worker-event-{}-{}-{}",
        lease.session_id(),
        lease.worker_generation(),
        incarnation,
    )));
    let mut stopping = false;
    let mut rescan_needed = false;

    loop {
        if active.is_none() && !stopping {
            if queue.is_empty() && rescan_needed {
                rescan_needed = refill_queued_turns(&lease, &mut queue, None).await;
            }
            while let Some(pending) = queue.pop_front() {
                let mut pending = pending;
                let run_id = pending.accepted.run_id.clone();
                let recovery_ready = pending.recovery_ready.take();
                let recovering = pending.recovering;
                match start_turn(
                    &dependencies,
                    &metadata,
                    &lease,
                    &device_id,
                    Arc::clone(&event_ids),
                    pending,
                )
                .await
                {
                    Ok(turn) => {
                        if let Some(ready) = recovery_ready {
                            let _ = ready.send(Ok(()));
                        }
                        active = Some(turn);
                        break;
                    }
                    Err(error) => {
                        if matches!(
                            durable_run_state(&lease, &run_id).await,
                            Some(RunState::Cancelling | RunState::Cancelled)
                        ) {
                            let terminalized = match cancelled_resumption_payloads(
                                &lease,
                                lease.session_id(),
                                &run_id,
                            )
                            .await
                            {
                                Ok(mut payloads) => {
                                    payloads.retain(|payload| {
                                        !matches!(payload, EventPayload::SessionState(_))
                                    });
                                    match append_payloads(
                                        &lease, &device_id, &run_id, &event_ids, payloads,
                                    )
                                    .await
                                    {
                                        Ok(()) => {
                                            append_session_idle(
                                                &lease, &device_id, &event_ids, true,
                                            )
                                            .await
                                        }
                                        Err(error) => Err(error),
                                    }
                                }
                                Err(error) => Err(error),
                            };
                            if let Some(ready) = recovery_ready {
                                let _ = ready.send(terminalized);
                            }
                        } else if recovering {
                            let terminalized = match failed_resumption_payloads(
                                &lease,
                                lease.session_id(),
                                &run_id,
                                &error,
                            )
                            .await
                            {
                                Ok(mut payloads) => {
                                    payloads.retain(|payload| {
                                        !matches!(payload, EventPayload::SessionState(_))
                                    });
                                    match append_payloads(
                                        &lease, &device_id, &run_id, &event_ids, payloads,
                                    )
                                    .await
                                    {
                                        Ok(()) => {
                                            append_session_idle(
                                                &lease, &device_id, &event_ids, true,
                                            )
                                            .await
                                        }
                                        Err(error) => Err(error),
                                    }
                                }
                                Err(terminalize_error) => Err(terminalize_error),
                            };
                            tracing::warn!(
                                session_id = %lease.session_id(),
                                %run_id,
                                ?error,
                                "recovered work could not resume and was terminalized"
                            );
                            if let Some(ready) = recovery_ready {
                                let _ = ready.send(terminalized);
                            }
                        } else {
                            let _ = append_failure(&lease, &device_id, &run_id, &event_ids, error)
                                .await;
                            let _ =
                                append_session_idle(&lease, &device_id, &event_ids, false).await;
                        }
                    }
                }
            }
        }

        if stopping && active.is_none() {
            break;
        }

        if let Some(turn) = active.as_mut() {
            let active_run = turn.run_id.clone();
            let active_cancel = turn.cancel.clone();
            tokio::select! {
                command = commands.recv() => {
                    match command {
                        Some(SupervisorCommand::Submit(pending)) => {
                            admit_pending(
                                &mut queue,
                                &mut rescan_needed,
                                &lease,
                                &device_id,
                                &event_ids,
                                Some(&active_run),
                                *pending,
                            ).await;
                        }
                        Some(SupervisorCommand::Shutdown) | None => {
                            stopping = true;
                            turn.cancel.cancel();
                            cancel_durable_queued_turns(
                                &mut queue,
                                &lease,
                                &device_id,
                                &event_ids,
                                Some(&active_run),
                            )
                            .await;
                        }
                    }
                }
                changed = cancellation_wakes.changed() => {
                    if changed.is_ok() {
                        reconcile_durable_cancellations(
                            &mut queue,
                            &lease,
                            &device_id,
                            &event_ids,
                            Some((&active_run, &active_cancel)),
                        ).await;
                    }
                }
                outcome = turn.outcome.as_mut() => {
                    if let Some(mut finished) = active.take() {
                        let (outcome_state, drive_error) = match outcome {
                            Ok(outcome) => (Some(outcome.state), None),
                            Err(error) => (None, Some(error)),
                        };
                        if let Some(dispatcher) = finished.dispatcher.take()
                            && let Err(error) = dispatcher.close().await
                        {
                            tracing::warn!(run_id = %finished.run_id, ?error, "turn tool dispatcher close failed");
                        }
                        let _ = finished.harness.stop().await;
                        let actor_panicked = if let Some(actor) = finished.actor.take() {
                            actor.await.is_err()
                        } else {
                            false
                        };
                        if drive_error.is_some() || actor_panicked {
                            let error = drive_error.unwrap_or_else(|| {
                                HaiderError::new(
                                    ErrorCode::Internal,
                                    "turn harness actor panicked",
                                    true,
                                )
                            });
                            if let Err(reconcile_error) = reconcile_unknown_effects(
                                &lease,
                                &device_id,
                                &finished.run_id,
                                &event_ids,
                            )
                            .await
                            {
                                tracing::error!(
                                    run_id = %finished.run_id,
                                    ?reconcile_error,
                                    "failed turn effect reconciliation blocked terminal commit"
                                );
                                let _ = lease.unregister_worker().await;
                                return false;
                            }
                            match failed_resumption_payloads(
                                &lease,
                                lease.session_id(),
                                &finished.run_id,
                                &error,
                            )
                            .await
                            {
                                Ok(mut payloads) => {
                                    payloads.retain(|payload| {
                                        !matches!(payload, EventPayload::SessionState(_))
                                    });
                                    if append_payloads(
                                        &lease,
                                        &device_id,
                                        &finished.run_id,
                                        &event_ids,
                                        payloads,
                                    )
                                    .await
                                    .is_ok()
                                    {
                                        let _ = append_session_idle(
                                            &lease, &device_id, &event_ids, true,
                                        )
                                        .await;
                                    }
                                }
                                Err(terminalize_error) => {
                                    tracing::error!(
                                        run_id = %finished.run_id,
                                        ?terminalize_error,
                                        "failed turn could not be terminalized"
                                    );
                                }
                            }
                            // Returning is intentional: the manager observes
                            // this JoinSet exit, evicts the slot, and retains
                            // the incarnation counter before a later submit.
                            let _ = lease.unregister_worker().await;
                            return true;
                        }
                        // TERMINAL ORDER: core cancellation is deliberately
                        // non-terminal in daemon mode. Broker close above first
                        // reconciles every held dispatch to Unknown; only then
                        // may Cancelled become the durable final envelope.
                        if let Err(error) = reconcile_unknown_effects(
                            &lease,
                            &device_id,
                            &finished.run_id,
                            &event_ids,
                        )
                        .await
                        {
                            // Never cross the terminal boundary while a
                            // Dispatched effect still lacks an outcome. A
                            // later startup/fresh supervisor may reconcile
                            // it, but this exit must not synthesize Cancelled.
                            tracing::error!(
                                run_id = %finished.run_id,
                                ?error,
                                "effect reconciliation failed; terminal commit remains fenced"
                            );
                            let _ = lease.unregister_worker().await;
                            return false;
                        }
                        let durable = durable_run_state(&lease, &finished.run_id).await;
                        let cancelled =
                            idle_interrupted_after_outcome(outcome_state.as_ref(), durable.as_ref());
                        if cancelled {
                            // Reduce durable lifecycle truth again after the
                            // harness stops. If core cancellation itself
                            // failed while closing an item/menu, finish those
                            // objects before the terminal boundary.
                            match cancelled_resumption_payloads(
                                &lease,
                                lease.session_id(),
                                &finished.run_id,
                            )
                            .await
                            {
                                Ok(mut payloads) => {
                                    payloads.retain(|payload| {
                                        !matches!(payload, EventPayload::SessionState(_))
                                    });
                                    let _ = append_payloads(
                                        &lease,
                                        &device_id,
                                        &finished.run_id,
                                        &event_ids,
                                        payloads,
                                    )
                                    .await;
                                }
                                Err(error) => {
                                    tracing::error!(
                                        run_id = %finished.run_id,
                                        ?error,
                                        "cancellation lifecycle reduction failed; terminal remains fenced"
                                    );
                                }
                            }
                        }
                        // Natural completion remains plain Idle even when it
                        // wins a drain race. Cancellation (user or drain) owns
                        // the interrupted marker.
                        let _ = append_session_idle(
                            &lease,
                            &device_id,
                            &event_ids,
                            cancelled,
                        )
                        .await;
                    }
                }
            }
        } else {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(SupervisorCommand::Submit(pending)) => {
                        admit_pending(
                            &mut queue,
                            &mut rescan_needed,
                            &lease,
                            &device_id,
                            &event_ids,
                            None,
                            *pending,
                        ).await;
                    }
                    Some(SupervisorCommand::Shutdown) | None => {
                        stopping = true;
                        let last = cancel_durable_queued_turns(
                            &mut queue,
                            &lease,
                            &device_id,
                            &event_ids,
                            None,
                        ).await;
                        if last.is_some() {
                            let _ = append_session_idle(&lease, &device_id, &event_ids, true)
                                .await;
                        }
                    }
                },
                changed = cancellation_wakes.changed() => {
                    if changed.is_ok() {
                        reconcile_durable_cancellations(
                            &mut queue,
                            &lease,
                            &device_id,
                            &event_ids,
                            None,
                        ).await;
                    }
                }
            }
        }
    }
    let _ = lease.unregister_worker().await;
    true
}

/// Chooses the one `Idle { interrupted }` meaning after a live outcome.
///
/// Drain state is intentionally absent: a turn whose durable terminal is
/// natural `Done` settles `false` even if drain began before its supervisor
/// observed the outcome. Only cancellation-shaped truth settles `true`.
fn idle_interrupted_after_outcome(
    outcome_state: Option<&RunState>,
    durable_state: Option<&RunState>,
) -> bool {
    matches!(outcome_state, Some(RunState::Cancelled))
        || matches!(durable_state, Some(RunState::Cancelling))
}

async fn admit_pending(
    queue: &mut VecDeque<PendingTurn>,
    rescan_needed: &mut bool,
    store: &HubStoreHandle,
    device_id: &DeviceId,
    event_ids: &EventIdGenerator,
    active_run: Option<&RunId>,
    mut pending: PendingTurn,
) {
    if pending.checkpoint.is_some() {
        if queue.len() < SUPERVISOR_CAPACITY {
            queue.push_back(pending);
        } else if let Some(ready) = pending.recovery_ready.take() {
            let _ = ready.send(Err(HaiderError::new(
                ErrorCode::Busy,
                "recovered checkpoint could not enter the bounded supervisor queue",
                true,
            )));
        }
        return;
    }
    let run_id = pending.accepted.run_id.clone();
    let state = durable_runs(store).await.ok().and_then(|runs| {
        runs.into_iter()
            .find_map(|(candidate, state, _)| (candidate == run_id).then_some(state))
    });
    match state {
        Some(RunState::Queued) => {
            if active_run == Some(&run_id)
                || queue.iter().any(|queued| queued.accepted.run_id == run_id)
            {
                if let Some(ready) = pending.recovery_ready.take() {
                    let _ = ready.send(Ok(()));
                }
                return;
            }
            if queue.len() < SUPERVISOR_CAPACITY {
                if let Some(ready) = pending.recovery_ready.take() {
                    // Handoff, not provider entry, is the Ready boundary for
                    // queued recovery. An earlier recovered checkpoint can
                    // remain parked indefinitely while this durable run waits
                    // safely in the owned supervisor.
                    let _ = ready.send(Ok(()));
                }
                queue.push_back(pending);
            } else if pending.recovering {
                let error = HaiderError::new(
                    ErrorCode::Busy,
                    "recovered queued turn exceeded the bounded supervisor queue",
                    true,
                );
                let terminalized =
                    match failed_resumption_payloads(store, store.session_id(), &run_id, &error)
                        .await
                    {
                        Ok(mut payloads) => {
                            payloads.retain(|payload| {
                                !matches!(payload, EventPayload::SessionState(_))
                            });
                            match append_payloads(store, device_id, &run_id, event_ids, payloads)
                                .await
                            {
                                Ok(()) => {
                                    append_session_idle(store, device_id, event_ids, true).await
                                }
                                Err(error) => Err(error),
                            }
                        }
                        Err(error) => Err(error),
                    };
                if let Some(ready) = pending.recovery_ready.take() {
                    let _ = ready.send(terminalized);
                }
            } else {
                // The durable Queued/UserMessage pair is the overflow buffer.
                // A later completion refills from the journal.
                *rescan_needed = true;
            }
        }
        Some(RunState::Cancelling) => {
            let terminalized =
                match cancelled_resumption_payloads(store, store.session_id(), &run_id).await {
                    Ok(mut payloads) => {
                        payloads
                            .retain(|payload| !matches!(payload, EventPayload::SessionState(_)));
                        match append_payloads(store, device_id, &run_id, event_ids, payloads).await
                        {
                            Ok(()) => append_session_idle(store, device_id, event_ids, true).await,
                            Err(error) => Err(error),
                        }
                    }
                    Err(error) => Err(error),
                };
            if let Some(ready) = pending.recovery_ready.take() {
                let _ = ready.send(terminalized);
            }
        }
        _ => {
            // Receipt replays for active or terminal runs are response-only.
            if let Some(ready) = pending.recovery_ready.take() {
                let _ = ready.send(Ok(()));
            }
        }
    }
}

async fn refill_queued_turns(
    store: &HubStoreHandle,
    queue: &mut VecDeque<PendingTurn>,
    active_run: Option<&RunId>,
) -> bool {
    let Ok(runs) = durable_runs(store).await else {
        return true;
    };
    let mut more = false;
    for (run_id, state, accepted_seq) in runs {
        if state != RunState::Queued
            || active_run == Some(&run_id)
            || queue
                .iter()
                .any(|pending| pending.accepted.run_id == run_id)
        {
            continue;
        }
        if queue.len() >= SUPERVISOR_CAPACITY {
            more = true;
            continue;
        }
        let Some(accepted_seq) = accepted_seq else {
            continue;
        };
        queue.push_back(PendingTurn::accepted(AcceptedTurn {
            session_id: store.session_id().clone(),
            run_id,
            accepted_seq,
            worker_generation: store.worker_generation(),
            disposition: haider_core::TurnAdmissionDisposition::Queued,
        }));
    }
    more
}

async fn reconcile_durable_cancellations(
    queue: &mut VecDeque<PendingTurn>,
    store: &HubStoreHandle,
    device_id: &DeviceId,
    event_ids: &EventIdGenerator,
    active: Option<(&RunId, &CancelToken)>,
) {
    let Ok(runs) = durable_runs(store).await else {
        return;
    };
    let active_run = active.map(|(run_id, _)| run_id);
    let mut terminalized = Vec::new();
    for (run_id, state, _) in runs {
        if state != RunState::Cancelling {
            continue;
        }
        if active_run == Some(&run_id) {
            if let Some((_, cancel)) = active {
                cancel.cancel();
            }
            continue;
        }
        if append_run_state(store, device_id, &run_id, event_ids, RunState::Cancelled)
            .await
            .is_ok()
        {
            terminalized.push(run_id);
        }
    }
    queue.retain(|pending| !terminalized.contains(&pending.accepted.run_id));
    if !terminalized.is_empty() {
        let _ = append_session_idle(store, device_id, event_ids, true).await;
    }
}

/// Reduces the committed journal to `(run, latest state, accepted seq)` in
/// acceptance order — the durable truth every admission/cancellation/refill
/// decision reads instead of trusting in-memory hints (module charter).
/// Its intentional O(journal) cost and projection trigger are ledgered in
/// `docs/OPTIMIZATIONS.md` under W3c1.
async fn durable_runs(
    store: &HubStoreHandle,
) -> Result<Vec<(RunId, RunState, Option<u64>)>, HaiderError> {
    let mut cursor = 0;
    let mut runs = HashMap::<RunId, (RunState, Option<u64>)>::new();
    loop {
        let page = StoreHandle::read(store, store.session_id(), cursor, 256).await?;
        if page.is_empty() {
            break;
        }
        cursor = page.last().map_or(cursor, |envelope| envelope.seq);
        for envelope in page {
            let Some(run_id) = envelope.run_id else {
                continue;
            };
            let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload) else {
                continue;
            };
            match payload {
                EventPayload::RunState(state) => {
                    let accepted = runs.get(&run_id).and_then(|(_, seq)| *seq);
                    runs.insert(run_id, (state, accepted));
                }
                EventPayload::UserMessage { .. } => {
                    let state = runs
                        .get(&run_id)
                        .map_or(RunState::Queued, |(state, _)| state.clone());
                    runs.insert(run_id, (state, Some(envelope.seq)));
                }
                _ => {}
            }
        }
    }
    let mut runs = runs
        .into_iter()
        .map(|(run_id, (state, accepted))| (run_id, state, accepted))
        .collect::<Vec<_>>();
    runs.sort_by_key(|(_, _, accepted)| accepted.unwrap_or(u64::MAX));
    Ok(runs)
}

/// Live counterpart of startup effect reconciliation, scoped to one run.
///
/// Dispatcher close is attempted first, but its return value is not evidence
/// that every held dispatch reached a terminal journal record. Durable truth
/// is reduced here and missing outcomes are appended before any run terminal.
async fn reconcile_unknown_effects(
    store: &HubStoreHandle,
    device_id: &DeviceId,
    run_id: &RunId,
    event_ids: &EventIdGenerator,
) -> Result<(), HaiderError> {
    let mut dispatched = HashSet::<EffectId>::new();
    let mut terminal = HashSet::<EffectId>::new();
    let mut cursor = 0;
    loop {
        let page = StoreHandle::read(store, store.session_id(), cursor, 512).await?;
        if page.is_empty() {
            break;
        }
        cursor = page.last().map_or(cursor, |envelope| envelope.seq);
        for envelope in page {
            if envelope.run_id.as_ref() != Some(run_id)
                || envelope
                    .payload
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    != Some("effect")
            {
                continue;
            }
            let payload =
                serde_json::from_value::<EventPayload>(envelope.payload).map_err(|error| {
                    HaiderError::new(
                        ErrorCode::StoreCorrupt,
                        format!(
                            "invalid effect payload in session {}, seq {}: {error}",
                            store.session_id(),
                            envelope.seq
                        ),
                        false,
                    )
                })?;
            match payload {
                EventPayload::Effect(EffectPhase::Dispatched { effect }) => {
                    dispatched.insert(effect);
                }
                EventPayload::Effect(EffectPhase::Outcome { effect, .. }) => {
                    terminal.insert(effect);
                }
                _ => {}
            }
        }
    }
    let mut pending = dispatched
        .difference(&terminal)
        .cloned()
        .collect::<Vec<_>>();
    pending.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    if pending.is_empty() {
        return Ok(());
    }
    append_payloads(
        store,
        device_id,
        run_id,
        event_ids,
        pending
            .into_iter()
            .map(|effect| {
                EventPayload::Effect(EffectPhase::Outcome {
                    effect,
                    outcome: EffectOutcome::Unknown,
                })
            })
            .collect(),
    )
    .await
}

async fn durable_run_state(store: &HubStoreHandle, run_id: &RunId) -> Option<RunState> {
    durable_runs(store).await.ok().and_then(|runs| {
        runs.into_iter()
            .find_map(|(candidate, state, _)| (candidate == *run_id).then_some(state))
    })
}

async fn cancel_durable_queued_turns(
    queue: &mut VecDeque<PendingTurn>,
    store: &HubStoreHandle,
    device_id: &DeviceId,
    event_ids: &EventIdGenerator,
    active_run: Option<&RunId>,
) -> Option<RunId> {
    let mut last = None;
    for (run_id, state, _) in durable_runs(store).await.unwrap_or_default() {
        if active_run == Some(&run_id) || state != RunState::Queued {
            continue;
        }
        if let Err(error) =
            append_run_state(store, device_id, &run_id, event_ids, RunState::Cancelled).await
        {
            tracing::warn!(%run_id, ?error, "queued turn could not be terminalized during drain");
        }
        last = Some(run_id);
    }
    while let Some(mut pending) = queue.pop_front() {
        if let Some(ready) = pending.recovery_ready.take() {
            let _ = ready.send(Err(HaiderError::new(
                ErrorCode::Busy,
                "daemon drained before recovered checkpoint could start",
                true,
            )));
        }
    }
    last
}

/// Assembles and starts one accepted turn: provider resolution (R6 pinning —
/// this is the once-per-logical-turn call), committed-history compilation
/// (R4), tool dispatcher creation, harness registration under the lease, and
/// submission.
///
/// Checkpoint resumption order is deliberate: the recovered harness
/// registers FIRST, then the journal is scanned for an already-committed
/// answer. An answer committed before registration is found by the scan; one
/// committed after the scan is delivered by the hub's registered-harness
/// wake; one committed between registration and the scan is sent TWICE (hub
/// wake at commit, then the scan's apply). The duplicate is safe because
/// both sends land in the harness's latest-value committed-menu watch before
/// the checkpoint turn's waiter performs its first read, which collapses
/// them into one observation — missing the answer is the failure mode this
/// ordering exists to prevent.
async fn start_turn(
    dependencies: &DaemonDependencies,
    metadata: &SessionMetadataV1,
    lease: &HubStoreHandle,
    device_id: &DeviceId,
    event_ids: Arc<EventIdGenerator>,
    pending: PendingTurn,
) -> Result<ActiveTurn, HaiderError> {
    let PendingTurn {
        accepted,
        checkpoint,
        mut committed_answer,
        recovery_ready: _,
        recovering: _,
    } = pending;
    let resolved = dependencies
        .provider_factory
        .resolve_for_turn(metadata)
        .await?;
    if resolved.provider_name != metadata.provider {
        return Err(HaiderError::new(
            ErrorCode::ProviderError,
            "provider factory returned a different provider than the session",
            false,
        ));
    }
    let prompt_compile_started = Instant::now();
    let mut messages =
        PromptHistoryCompiler::compile(lease, lease.session_id(), None, None, &accepted.run_id)
            .await?;
    tracing::trace!(
        target: "haider.worker",
        session_id = %lease.session_id(),
        run_id = %accepted.run_id,
        prompt_messages = messages.len(),
        compile_micros = prompt_compile_started.elapsed().as_micros(),
        "prompt history compiled"
    );
    let attachments = resolve_prompt_attachments(lease, &mut messages).await?;
    let dispatcher = dependencies
        .tool_factory
        .create(WorkerToolContext {
            metadata: metadata.clone(),
            store: lease.clone(),
            run_id: accepted.run_id.clone(),
            device_id: device_id.clone(),
            event_ids: Arc::clone(&event_ids),
        })
        .await?;
    let mut config = HarnessConfig::for_session(
        lease.session_id().clone(),
        device_id.clone(),
        0,
        lease.worker_generation(),
    )
    .with_event_ids(event_ids);
    config.model = resolved.model;
    config.max_tokens = metadata.max_tokens;
    config.system_prompt = Some(SystemPromptBuilder::build(metadata));
    config.tools = dependencies.tool_factory.definitions();
    config.attachments = attachments;
    config.usage_account = resolved
        .account_alias
        .map(haider_protocol::ids::CredentialAlias::new);
    config.supervisor_commits_cancelled = true;
    // Last uncancellable startup boundary: provider/tool resolution is done,
    // but the harness actor has not been spawned or submitted. A cancellation
    // committed while either factory was awaited aborts here. The worker
    // append transition gate remains the atomic backstop for a later tie.
    if cancellation_fences_start(durable_run_state(lease, &accepted.run_id).await) {
        if let Some(dispatcher) = dispatcher.as_ref() {
            let _ = dispatcher.close().await;
        }
        return Err(cancellation_fenced_start());
    }
    let (actor, harness) = HarnessActor::new_with_dispatcher(
        config,
        resolved.provider,
        Arc::new(lease.clone()),
        dispatcher.clone(),
    );
    match checkpoint.as_ref() {
        Some(checkpoint) => {
            lease
                .register_recovered_harness(
                    harness.clone(),
                    checkpoint.menu.id.clone(),
                    checkpoint.request_seq,
                    checkpoint.opening_generation,
                )
                .await
                .map_err(hub_error)?;
        }
        None => {
            lease
                .register_harness(harness.clone())
                .await
                .map_err(hub_error)?;
        }
    }
    if committed_answer.is_none()
        && let Some(checkpoint) = checkpoint.as_ref()
    {
        committed_answer = find_committed_menu_answer(lease, &checkpoint.menu.id).await?;
    }
    if let Some(answer) = committed_answer {
        harness.apply_committed_menu_event(answer)?;
    }
    let actor = AbortOnDropTask::new(tokio::spawn(actor.run()));
    let submitted = match checkpoint {
        Some(checkpoint) => {
            harness
                .submit_checkpoint_turn(SubmitCheckpointTurn {
                    run_id: accepted.run_id.clone(),
                    messages,
                    checkpoint,
                })
                .await
        }
        None => {
            harness
                .submit_committed_turn(SubmitCommittedTurn {
                    run_id: accepted.run_id.clone(),
                    messages,
                })
                .await
        }
    };
    let handle = submitted?;
    Ok(active_turn(
        accepted.run_id,
        harness,
        actor.into_inner(),
        dispatcher,
        handle,
    ))
}

async fn find_committed_menu_answer(
    store: &HubStoreHandle,
    menu_id: &haider_protocol::ids::MenuId,
) -> Result<Option<haider_protocol::envelope::RawEnvelope>, HaiderError> {
    let mut cursor = 0;
    loop {
        let page = StoreHandle::read(store, store.session_id(), cursor, 256).await?;
        if page.is_empty() {
            return Ok(None);
        }
        cursor = page.last().map_or(cursor, |envelope| envelope.seq);
        if let Some(answer) = page.into_iter().find(|envelope| {
            serde_json::from_value::<EventPayload>(envelope.payload.clone()).is_ok_and(
                |payload| matches!(payload, EventPayload::MenuAnswered(answer) if answer.menu == *menu_id),
            )
        }) {
            return Ok(Some(answer));
        }
    }
}

async fn resolve_prompt_attachments(
    store: &HubStoreHandle,
    messages: &mut [Message],
) -> Result<Vec<ResolvedAttachment>, HaiderError> {
    let mut resolved = Vec::<ResolvedAttachment>::new();
    for message in messages {
        for block in &mut message.blocks {
            match block {
                haider_protocol::provider::Block::Attachment(
                    haider_protocol::tool::AttachmentBlock::Image { artifact, .. },
                ) => {
                    if resolved
                        .iter()
                        .any(|attachment| attachment.artifact.as_str() == artifact.as_str())
                    {
                        continue;
                    }
                    let artifact = artifact.clone();
                    let bytes = store.get_artifact(artifact.clone()).await?;
                    resolved.push(ResolvedAttachment {
                        artifact,
                        data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
                    });
                }
                haider_protocol::provider::Block::Attachment(
                    haider_protocol::tool::AttachmentBlock::PastedText { artifact, .. },
                ) => {
                    let artifact = artifact.clone();
                    let bytes = store.get_artifact(artifact.clone()).await?;
                    let text = String::from_utf8(bytes).map_err(|_| {
                        HaiderError::new(
                            ErrorCode::InvalidArgument,
                            format!("pasted-text attachment {artifact} is not UTF-8"),
                            false,
                        )
                    })?;
                    *block = haider_protocol::provider::Block::Text { text };
                }
                haider_protocol::provider::Block::Attachment(
                    haider_protocol::tool::AttachmentBlock::Skill { name, .. },
                ) => {
                    let name = name.clone();
                    return Err(HaiderError::new(
                        ErrorCode::InvalidArgument,
                        format!("skill attachment `{name}` is reserved but not yet supported"),
                        false,
                    ));
                }
                _ => {}
            }
        }
    }
    Ok(resolved)
}

fn active_turn(
    run_id: RunId,
    harness: haider_core::HarnessHandle,
    actor: JoinHandle<()>,
    dispatcher: Option<Arc<dyn ToolDispatcher>>,
    handle: TurnHandle,
) -> ActiveTurn {
    let cancel = handle.cancel_token();
    ActiveTurn {
        run_id,
        cancel,
        outcome: Box::pin(handle.wait()),
        harness,
        dispatcher,
        actor: Some(actor),
    }
}

struct AbortOnDropTask(Option<JoinHandle<()>>);

impl AbortOnDropTask {
    fn new(task: JoinHandle<()>) -> Self {
        Self(Some(task))
    }

    fn into_inner(mut self) -> JoinHandle<()> {
        let Some(task) = self.0.take() else {
            unreachable!("abort-on-drop task can be disarmed only once");
        };
        task
    }
}

impl Drop for AbortOnDropTask {
    fn drop(&mut self) {
        if let Some(task) = &self.0 {
            task.abort();
        }
    }
}

async fn append_failure(
    store: &HubStoreHandle,
    device_id: &DeviceId,
    run_id: &RunId,
    event_ids: &EventIdGenerator,
    error: HaiderError,
) -> Result<(), HaiderError> {
    append_payloads(
        store,
        device_id,
        run_id,
        event_ids,
        vec![
            EventPayload::RunFailed {
                code: error.code,
                message: sanitized_failure_message(&error.message),
                retryable: error.retryable,
            },
            EventPayload::RunState(RunState::Errored),
        ],
    )
    .await
}

async fn append_run_state(
    store: &HubStoreHandle,
    device_id: &DeviceId,
    run_id: &RunId,
    event_ids: &EventIdGenerator,
    state: RunState,
) -> Result<(), HaiderError> {
    append_payloads(
        store,
        device_id,
        run_id,
        event_ids,
        vec![EventPayload::RunState(state)],
    )
    .await
}

/// Offers the aggregate `Idle` settle. Deliberately run-agnostic: aggregate
/// `SessionState` envelopes carry no run id, and whether Idle actually
/// commits is decided durably by `Store::settle_session_idle` (all runs
/// terminal), never by this caller's view of which run just finished.
async fn append_session_idle(
    store: &HubStoreHandle,
    device_id: &DeviceId,
    event_ids: &EventIdGenerator,
    interrupted: bool,
) -> Result<(), HaiderError> {
    let envelope = supervisor_envelope(
        store,
        device_id,
        None,
        event_ids.next(),
        EventPayload::SessionState(SessionState::Idle { interrupted }),
    )?;
    store.settle_idle(envelope).await.map(|_| ())
}

async fn append_payloads(
    store: &HubStoreHandle,
    device_id: &DeviceId,
    run_id: &RunId,
    event_ids: &EventIdGenerator,
    payloads: Vec<EventPayload>,
) -> Result<(), HaiderError> {
    let mut envelopes = Vec::with_capacity(payloads.len());
    for payload in payloads {
        let payload_run_id =
            (!matches!(payload, EventPayload::SessionState(_))).then(|| run_id.clone());
        envelopes.push(supervisor_envelope(
            store,
            device_id,
            payload_run_id,
            event_ids.next(),
            payload,
        )?);
    }
    haider_core::StoreHandle::append(store, &mut envelopes).await?;
    Ok(())
}

fn supervisor_envelope(
    store: &HubStoreHandle,
    device_id: &DeviceId,
    run_id: Option<RunId>,
    event_id: EventId,
    payload: EventPayload,
) -> Result<haider_protocol::envelope::RawEnvelope, HaiderError> {
    Ok(EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id,
        seq: 0,
        session_id: store.session_id().clone(),
        branch_id: None,
        run_id,
        agent_id: None,
        device_id: device_id.clone(),
        authority_epoch: 0,
        worker_generation: store.worker_generation(),
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::to_value(payload).map_err(|error| {
            HaiderError::new(
                ErrorCode::Internal,
                format!("cannot serialize supervisor event: {error}"),
                false,
            )
        })?,
    })
}

fn manager_stopped() -> HaiderError {
    HaiderError::new(ErrorCode::Internal, "worker manager is not running", true)
}

fn manager_busy(message: &str) -> HaiderError {
    HaiderError::new(ErrorCode::Busy, message, true)
}

fn manager_try_send(error: mpsc::error::TrySendError<ManagerCommand>) -> HaiderError {
    match error {
        mpsc::error::TrySendError::Full(_) => manager_busy("worker manager queue is full"),
        mpsc::error::TrySendError::Closed(_) => manager_stopped(),
    }
}

fn supervisor_try_send(error: mpsc::error::TrySendError<SupervisorCommand>) -> HaiderError {
    match error {
        mpsc::error::TrySendError::Full(_) => manager_busy("session worker queue is full"),
        mpsc::error::TrySendError::Closed(_) => manager_stopped(),
    }
}

fn cancellation_fenced_start() -> HaiderError {
    HaiderError::new(
        ErrorCode::RunNotActive,
        "turn start was fenced by durable cancellation",
        false,
    )
}

fn cancellation_fences_start(state: Option<RunState>) -> bool {
    matches!(state, Some(RunState::Cancelling | RunState::Cancelled))
}

fn hub_error(error: SessionHubError) -> HaiderError {
    HaiderError::new(ErrorCode::Internal, error.to_string(), true)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::items_after_test_module)]
mod manager_law_tests {
    use super::*;

    #[test]
    fn full_manager_queue_maps_to_typed_busy_without_waiting() {
        let (commands, _receiver) = mpsc::channel(1);
        let (first, _first_response) = oneshot::channel();
        commands
            .try_send(ManagerCommand::Shutdown { completed: first })
            .expect("fills manager queue");
        let (second, _second_response) = oneshot::channel();
        let error = commands
            .try_send(ManagerCommand::Shutdown { completed: second })
            .map_err(manager_try_send)
            .expect_err("full queue rejects immediately");
        assert_eq!(error.code, ErrorCode::Busy);
        assert!(error.retryable);
    }

    #[test]
    fn runtime_closes_worker_admission_before_hub_drain() {
        let runtime = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/runtime.rs"),
        )
        .expect("runtime source");
        let worker = runtime
            .find("worker_handle.begin_draining();")
            .expect("worker admission gate");
        let hub = runtime
            .find("hub.begin_draining();")
            .expect("hub admission gate");
        assert!(worker < hub);
    }

    #[test]
    fn naturally_done_turn_stays_uninterrupted_when_drain_observes_it_late() {
        assert!(!idle_interrupted_after_outcome(
            Some(&RunState::Done),
            Some(&RunState::Done),
        ));
        assert!(idle_interrupted_after_outcome(
            Some(&RunState::Cancelled),
            Some(&RunState::Cancelling),
        ));
    }

    #[test]
    fn durable_cancelling_fences_the_last_harness_start_boundary() {
        assert!(cancellation_fences_start(Some(RunState::Cancelling)));
        assert!(cancellation_fences_start(Some(RunState::Cancelled)));
        assert!(!cancellation_fences_start(Some(RunState::Queued)));
    }
}

// ───────────────── production broker-backed general tools ─────────────────

struct BrokerToolFactory;

#[async_trait]
impl TurnToolFactory for BrokerToolFactory {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![
            request_input_definition(),
            tool_definition("fs_read", "Read a UTF-8 file", &["path"]),
            tool_definition("fs_list", "List a directory", &["path"]),
            tool_definition("fs_search", "Search files for text", &["root", "query"]),
        ]
    }

    async fn create(
        &self,
        context: WorkerToolContext,
    ) -> Result<Option<Arc<dyn ToolDispatcher>>, HaiderError> {
        let journal = HubJournalSink::new(&context);
        let broker = EffectBroker::new(
            Box::new(journal),
            &context.metadata.cwd,
            context.store.session_id().clone(),
            context.store.worker_generation(),
        )
        .map_err(tool_error)?;
        let mut policy = PermissionPolicy::default();
        policy.allow(EffectClass::FsRead);
        Ok(Some(Arc::new(BrokerToolDispatcher {
            broker: Mutex::new(Some(broker)),
            policy,
            cas: Mutex::new(HubArtifactStore {
                store: context.store,
            }),
        })))
    }
}

struct BrokerToolDispatcher {
    broker: Mutex<Option<EffectBroker>>,
    policy: PermissionPolicy,
    cas: Mutex<HubArtifactStore>,
}

#[async_trait]
impl ToolDispatcher for BrokerToolDispatcher {
    async fn execute(
        &self,
        _run_id: &RunId,
        _call_id: &str,
        name: &str,
        args: serde_json::Value,
        cancel: &CancelToken,
    ) -> Result<BoundedResult, HaiderError> {
        if cancel.is_cancelled() {
            return Err(HaiderError::new(
                ErrorCode::Internal,
                "tool dispatch was cancelled before start",
                false,
            ));
        }
        let mut broker = self.broker.lock().await;
        let broker = broker.as_mut().ok_or_else(|| {
            HaiderError::new(
                ErrorCode::Internal,
                "tool dispatcher is already closed",
                false,
            )
        })?;
        let mut cas = self.cas.lock().await;
        let result = match name {
            "fs_read" => {
                let path = required_string(&args, "path")?;
                broker
                    .fs_read(
                        &FsRead::new(path),
                        &self.policy,
                        &mut *cas,
                        ResultBounds::default(),
                    )
                    .await
            }
            "fs_list" => {
                let path = required_string(&args, "path")?;
                broker
                    .fs_list(
                        &FsList::new(path),
                        &self.policy,
                        &mut *cas,
                        ResultBounds::default(),
                    )
                    .await
            }
            "fs_search" => {
                let root = required_string(&args, "root")?;
                let query = required_string(&args, "query")?;
                broker
                    .fs_search(
                        &FsSearch::new(root, query),
                        &self.policy,
                        &mut *cas,
                        ResultBounds::default(),
                    )
                    .await
            }
            _ => {
                return Err(HaiderError::new(
                    ErrorCode::InvalidArgument,
                    format!("unsupported tool `{name}`"),
                    false,
                ));
            }
        };
        result.map_err(tool_error)
    }

    async fn close(&self) -> Result<(), HaiderError> {
        let broker = self.broker.lock().await.take();
        let Some(broker) = broker else {
            return Ok(());
        };
        broker.close().await.map(|_| ()).map_err(|error| {
            HaiderError::new(
                ErrorCode::EffectUnknownOutcome,
                format!("effect broker close reported unfinished work: {error}"),
                false,
            )
        })
    }
}

fn request_input_definition() -> ToolDefinition {
    ToolDefinition {
        name: "request_input".into(),
        description: "Ask the user one blocking question or a server-enumerated choice".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "kind": {"type": "string", "enum": ["question", "choice"]},
                "title": {"type": "string", "minLength": 1},
                "body": {"type": "array", "items": {"type": "string"}},
                "options": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "key": {"type": "string", "minLength": 1},
                            "label": {"type": "string", "minLength": 1},
                            "detail": {"type": "string"}
                        },
                        "required": ["key", "label"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["kind", "title"],
            "additionalProperties": false
        }),
    }
}

fn tool_definition(name: &str, description: &str, required: &[&str]) -> ToolDefinition {
    let properties = required
        .iter()
        .map(|name| ((*name).to_owned(), serde_json::json!({"type": "string"})))
        .collect::<serde_json::Map<_, _>>();
    ToolDefinition {
        name: name.into(),
        description: description.into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        }),
    }
}

fn required_string(args: &serde_json::Value, field: &str) -> Result<String, HaiderError> {
    args.get(field)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            HaiderError::new(
                ErrorCode::InvalidArgument,
                format!("tool argument `{field}` must be a non-empty string"),
                false,
            )
        })
}

struct HubArtifactStore {
    store: HubStoreHandle,
}

#[async_trait]
impl CasSink for HubArtifactStore {
    async fn put(&mut self, bytes: &[u8]) -> ToolResult<haider_protocol::ids::ArtifactRef> {
        self.store
            .put_artifact(bytes.to_vec())
            .await
            .map_err(|error| haider_tools::ToolError::Runtime {
                message: error.message,
            })
    }

    async fn put_file(&mut self, path: &Path) -> ToolResult<haider_protocol::ids::ArtifactRef> {
        self.store
            .put_artifact_file(path.to_path_buf())
            .await
            .map_err(|error| haider_tools::ToolError::Runtime {
                message: error.message,
            })
    }
}

struct HubJournalSink {
    store: HubStoreHandle,
    run_id: RunId,
    device_id: DeviceId,
    event_ids: Arc<EventIdGenerator>,
}

impl HubJournalSink {
    fn new(context: &WorkerToolContext) -> Self {
        Self {
            store: context.store.clone(),
            run_id: context.run_id.clone(),
            device_id: context.device_id.clone(),
            event_ids: Arc::clone(&context.event_ids),
        }
    }
}

#[async_trait]
impl JournalSink for HubJournalSink {
    async fn append(&mut self, payload: EventPayload) -> ToolResult<()> {
        let mut envelopes = [EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: self.event_ids.next(),
            seq: 0,
            session_id: self.store.session_id().clone(),
            branch_id: None,
            run_id: Some(self.run_id.clone()),
            agent_id: None,
            device_id: self.device_id.clone(),
            authority_epoch: 0,
            worker_generation: self.store.worker_generation(),
            causation_id: None,
            correlation_id: None,
            committed_at_ms: 0,
            render: RenderTargets {
                ui: true,
                durable: true,
                prompt: PromptRender::Omit,
            },
            payload: serde_json::to_value(payload).map_err(|error| {
                haider_tools::ToolError::Runtime {
                    message: format!("cannot serialize effect envelope: {error}"),
                }
            })?,
        }];
        haider_core::StoreHandle::append(&self.store, &mut envelopes)
            .await
            .map_err(|error| haider_tools::ToolError::Runtime {
                message: error.message,
            })?;
        Ok(())
    }
}

fn tool_error(error: haider_tools::ToolError) -> HaiderError {
    HaiderError::new(ErrorCode::ProviderError, error.to_string(), false)
}
