//! CHARTER — the turn engine: owned per-session supervisors and injectable
//! turn dependencies (report R1).
//!
//! What lives here: [`WorkerManager`] (one lazy supervisor per session, all
//! tasks owned — nothing detached), the supervisor loop (accepted-turn
//! queue, active cancellation, provider/tool/prompt assembly, drain
//! settlement), the turn-scoped [`ProviderFactory`]/[`TurnToolFactory`]
//! ports, and the production broker-backed tool dispatcher with its
//! hub-owned journal/CAS adapters. What may NOT live here: SQLite (a worker
//! holds only its lease-fenced `HubStoreHandle`; this module never names
//! `SqliteStoreHandle` — grep-enforced, the module-side half of the R1
//! append-exclusivity seal), wire/RPC concerns
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
use async_trait::async_trait;
use base64::Engine;
use haider_core::{
    AcceptedTurn, CancelToken, EventIdGenerator, HarnessActor, HarnessConfig,
    PromptHistoryCompiler, RequestInputCheckpoint, StoreHandle, SubmitCheckpointTurn,
    SubmitCommittedTurn, ToolDispatcher, TurnHandle, sanitized_failure_message,
};
use haider_protocol::EventPayload;
use haider_protocol::effect::EffectClass;
use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::ids::{DeviceId, EventId, RunId, SessionId};
use haider_protocol::session::SessionMetadataV1;
use haider_protocol::state::{RunState, SessionState};
use haider_protocol::tool::BoundedResult;
use haider_provider::{Message, ResolvedAttachment};
use haider_provider::{Provider, ToolDefinition};
use haider_tools::{
    CasSink, EffectBroker, FsList, FsRead, FsSearch, JournalSink, PermissionPolicy, ResultBounds,
    ToolResult,
};
use std::collections::{HashMap, VecDeque};
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
}

impl Default for DaemonDependencies {
    fn default() -> Self {
        Self {
            provider_factory: Arc::new(UnconfiguredProviderFactory),
            tool_factory: Arc::new(BrokerToolFactory),
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
    RecoverCheckpoint {
        pending: Box<PendingTurn>,
    },
    Shutdown {
        completed: oneshot::Sender<()>,
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
}

impl PendingTurn {
    fn accepted(accepted: AcceptedTurn) -> Self {
        Self {
            accepted,
            checkpoint: None,
            committed_answer: None,
            recovery_ready: None,
        }
    }
}

impl WorkerManager {
    pub(crate) fn start(hub: SessionHub, dependencies: DaemonDependencies) -> Self {
        let (commands, receiver) = mpsc::channel(MANAGER_CAPACITY);
        let handle = WorkerManagerHandle { commands };
        let task = tokio::spawn(run_manager(hub, dependencies, receiver));
        Self {
            handle,
            task: Some(task),
        }
    }

    pub(crate) fn handle(&self) -> WorkerManagerHandle {
        self.handle.clone()
    }

    pub(crate) async fn shutdown(mut self) {
        let (completed, response) = oneshot::channel();
        let _ = self
            .handle
            .commands
            .send(ManagerCommand::Shutdown { completed })
            .await;
        let _ = response.await;
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }

    /// Abrupt owner teardown for the in-process process-death seam.
    ///
    /// Unlike `shutdown`, this sends no cancellation command and appends no
    /// terminal event. Startup recovery must decide what the durable prefix
    /// means.
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
    pub(crate) async fn submit(&self, accepted: AcceptedTurn) -> Result<(), HaiderError> {
        let (completed, response) = oneshot::channel();
        self.commands
            .send(ManagerCommand::Submit {
                accepted,
                completed,
            })
            .await
            .map_err(|_| manager_stopped())?;
        response.await.map_err(|_| manager_stopped())?
    }

    pub(crate) async fn recover_checkpoint(
        &self,
        accepted: AcceptedTurn,
        checkpoint: RequestInputCheckpoint,
        committed_answer: Option<haider_protocol::envelope::RawEnvelope>,
    ) -> Result<(), HaiderError> {
        let (completed, response) = oneshot::channel();
        self.commands
            .send(ManagerCommand::RecoverCheckpoint {
                pending: Box::new(PendingTurn {
                    accepted,
                    checkpoint: Some(checkpoint),
                    committed_answer,
                    recovery_ready: Some(completed),
                }),
            })
            .await
            .map_err(|_| manager_stopped())?;
        response.await.map_err(|_| manager_stopped())?
    }
}

async fn run_manager(
    hub: SessionHub,
    dependencies: DaemonDependencies,
    mut commands: mpsc::Receiver<ManagerCommand>,
) {
    let mut supervisors = HashMap::<SessionId, mpsc::Sender<SupervisorCommand>>::new();
    let mut tasks = JoinSet::new();
    while let Some(command) = commands.recv().await {
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
                    accepted.session_id.clone(),
                )
                .await
                {
                    Ok(supervisor) => supervisor
                        .send(SupervisorCommand::Submit(Box::new(PendingTurn::accepted(
                            accepted,
                        ))))
                        .await
                        .map_err(|_| manager_stopped()),
                    Err(error) => Err(error),
                };
                let _ = completed.send(result);
            }
            ManagerCommand::RecoverCheckpoint { mut pending } => {
                let session_id = pending.accepted.session_id.clone();
                match supervisor_for(
                    &hub,
                    &dependencies,
                    &mut supervisors,
                    &mut tasks,
                    session_id,
                )
                .await
                {
                    Ok(supervisor) => {
                        if let Err(error) =
                            supervisor.send(SupervisorCommand::Submit(pending)).await
                        {
                            let SupervisorCommand::Submit(mut pending) = error.0 else {
                                unreachable!();
                            };
                            if let Some(ready) = pending.recovery_ready.take() {
                                let _ = ready.send(Err(manager_stopped()));
                            }
                        }
                    }
                    Err(error) => {
                        if let Some(ready) = pending.recovery_ready.take() {
                            let _ = ready.send(Err(error));
                        }
                    }
                }
            }
            ManagerCommand::Shutdown { completed } => {
                for supervisor in supervisors.values() {
                    let _ = supervisor.send(SupervisorCommand::Shutdown).await;
                }
                while tasks.join_next().await.is_some() {}
                let _ = completed.send(());
                return;
            }
        }
        while tasks.try_join_next().is_some() {}
    }
    for supervisor in supervisors.values() {
        let _ = supervisor.send(SupervisorCommand::Shutdown).await;
    }
    while tasks.join_next().await.is_some() {}
}

async fn supervisor_for(
    hub: &SessionHub,
    dependencies: &DaemonDependencies,
    supervisors: &mut HashMap<SessionId, mpsc::Sender<SupervisorCommand>>,
    tasks: &mut JoinSet<()>,
    session_id: SessionId,
) -> Result<mpsc::Sender<SupervisorCommand>, HaiderError> {
    if let Some(supervisor) = supervisors.get(&session_id) {
        return Ok(supervisor.clone());
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
    tasks.spawn(run_supervisor(
        hub.clone(),
        dependencies.clone(),
        metadata,
        lease,
        receiver,
        cancellation_wakes,
    ));
    supervisors.insert(session_id, sender.clone());
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
    hub: SessionHub,
    dependencies: DaemonDependencies,
    metadata: SessionMetadataV1,
    lease: HubStoreHandle,
    mut commands: mpsc::Receiver<SupervisorCommand>,
    mut cancellation_wakes: tokio::sync::watch::Receiver<u64>,
) {
    let mut queue = VecDeque::<PendingTurn>::new();
    let mut active: Option<ActiveTurn> = None;
    let device_id = DeviceId::new(format!(
        "worker-{}-{}",
        lease.session_id(),
        lease.worker_generation()
    ));
    let event_ids = Arc::new(EventIdGenerator::new(format!(
        "worker-event-{}-{}",
        lease.session_id(),
        lease.worker_generation()
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
                match start_turn(
                    &hub,
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
                        if let Some(ready) = recovery_ready {
                            let _ = ready.send(Err(error.clone()));
                        }
                        let _ =
                            append_failure(&lease, &device_id, &run_id, &event_ids, error).await;
                        let _ = append_session_idle(&lease, &device_id, &event_ids, false).await;
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
                _outcome = turn.outcome.as_mut() => {
                    if let Some(mut finished) = active.take() {
                        if let Some(dispatcher) = finished.dispatcher.take()
                            && let Err(error) = dispatcher.close().await
                        {
                            tracing::warn!(run_id = %finished.run_id, ?error, "turn tool dispatcher close failed");
                        }
                        let _ = finished.harness.stop().await;
                        if let Some(actor) = finished.actor.take() {
                            let _ = actor.await;
                        }
                        let _ = append_session_idle(&lease, &device_id, &event_ids, stopping)
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
    let _ = hub.unregister_worker(&lease).await;
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
                return;
            }
            if queue.len() < SUPERVISOR_CAPACITY {
                queue.push_back(pending);
            } else {
                // The durable Queued/UserMessage pair is the overflow buffer.
                // A later completion refills from the journal.
                *rescan_needed = true;
            }
        }
        Some(RunState::Cancelling) => {
            let _ =
                append_run_state(store, device_id, &run_id, event_ids, RunState::Cancelled).await;
            let _ = append_session_idle(store, device_id, event_ids, false).await;
        }
        _ => {
            // Receipt replays for active or terminal runs are response-only.
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
        let _ = append_session_idle(store, device_id, event_ids, false).await;
    }
}

/// Reduces the committed journal to `(run, latest state, accepted seq)` in
/// acceptance order — the durable truth every admission/cancellation/refill
/// decision reads instead of trusting in-memory hints (module charter).
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
    hub: &SessionHub,
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
    let (actor, harness) = HarnessActor::new_with_dispatcher(
        config,
        resolved.provider,
        Arc::new(lease.clone()),
        dispatcher.clone(),
    );
    match checkpoint.as_ref() {
        Some(checkpoint) => {
            hub.register_recovered_harness(
                lease,
                harness.clone(),
                checkpoint.menu.id.clone(),
                checkpoint.request_seq,
                checkpoint.opening_generation,
            )
            .await
            .map_err(hub_error)?;
        }
        None => {
            hub.register_leased_harness(lease, harness.clone())
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

fn hub_error(error: SessionHubError) -> HaiderError {
    HaiderError::new(ErrorCode::Internal, error.to_string(), true)
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
