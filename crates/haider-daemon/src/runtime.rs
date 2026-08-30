//! Top-level lifecycle ordering and task ownership.
//!
//! [`run_inner`] is the one place that sequences the daemon's life. Its
//! ordering is load-bearing (d1 report R1/R16/R17) and must not be reordered:
//!
//! 1. validate config — nothing touched yet;
//! 2. acquire the profile lifetime lock (R1) — BEFORE socket cleanup or
//!    store open; losing this race is the typed `AlreadyRunning` exit;
//! 3. open the store under the lock, durably bump the daemon generation,
//!    and run C4a reconciliation for every dispatched-without-terminal
//!    effect (R16) — no listener exists yet, so nothing can observe
//!    half-recovered state;
//! 4. bind the endpoint, then publish `Ready` (unless shutdown already
//!    intervened) and serve the accept loop — every served connection holds one
//!    admission permit, and a peer accepted beyond `max_connections` is
//!    answered `overloaded` and closed without becoming a task;
//! 5. drain (R17): close the listener, publish `Draining`, broadcast
//!    `ServerDraining`, wait bounded for connections, flush the store,
//!    remove the exact owned socket, and close the store LAST — closing
//!    the store is what releases the profile lock. One deadline bounds every
//!    abortable stage, and a second signal forces at any point in it. An
//!    already-running, non-abortable account-vault write is joined past that
//!    deadline when necessary; `Stopped` and the profile lock are withheld
//!    until its rotated value is restored or durably failed closed.
//!
//! Overrun semantics, stated once: a blocking SQLite call cannot be cancelled,
//! so an overrunning flush/close is STARTED and then abandoned — this task
//! stops waiting, reports `Forced`, and the abandoned call releases the profile
//! lock the moment it returns. A caller that restarts the same profile
//! immediately after a `Forced` outcome may still meet `AlreadyRunning` for
//! that moment; that is the honest report of a degraded shutdown, not a leak.
//! Account-vault persistence is stricter because it carries token bytes: it is
//! never abandoned, and no `Stopped` outcome is published before it joins.
//!
//! Shutdown may arrive at any point; the early-exit helpers
//! ([`shutdown_without_store`], [`shutdown_before_listener`]) run the same
//! tail ordering with whatever resources exist so far.
//!
//! The phases above are separate functions ([`reconcile_before_ready`],
//! [`ConnectionRuntime::accept_until_shutdown`], [`ConnectionRuntime::drain`],
//! [`finalize`]), each with exactly ONE call site, written in the same order
//! they run and in that order in this file. The split moved no statement across
//! a phase boundary: [`run_inner`] still reads as the sequence, which is the
//! contract.

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod runtime_tests;

use crate::connection::{ConnectionContext, ConnectionExit, DrainNotice, reject_over_limit, serve};
use crate::diagnostics::EffectDiagnostics;
use crate::endpoint;
use crate::hooks::HookStartupHydrator;
use crate::lifecycle::{ShutdownObserver, ShutdownReason, ShutdownRequest, StatePublisher};
use crate::pipe_native::{PipeBootSession, PipeNativeWriter};
use crate::turn_recovery::{
    RecoveredWork, StartupJournalVisitor, recover_interrupted_turns_report_with_visitor,
};
use crate::worker::WorkerManager;
use crate::{
    DaemonConfig, DaemonDependencies, DaemonError, DaemonState, IncumbentDiagnostics, Readiness,
    SessionHub, ShutdownHandle, ShutdownOutcome,
};
use haider_core::{SqliteStoreHandle, StoreHandle, reconcile_dispatched_effects};
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::ids::{DeviceId, SessionId};
use std::fs;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::Poll;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{Semaphore, watch};
use tokio::task::{JoinHandle, JoinSet};

const STARTUP_JOURNAL_PAGE_ENVELOPES: usize = 512;
const STARTUP_JOURNAL_PAGE_BYTES: usize = 4 * 1_024 * 1_024;
const STARTUP_VISITOR_PAYLOAD_KINDS: &[&str] = &[
    "effect",
    "hook_run_trust",
    "item",
    "item_tool_call",
    "menu_answered",
    "menu_closed",
    "menu_opened",
    "node_committed",
    "run_failed",
    "run_state",
    "tool_result",
];

/// Handle to one spawned daemon: observe its phases, request shutdown, and
/// join for the typed outcome.
pub struct DaemonTask {
    readiness: Readiness,
    shutdown: ShutdownHandle,
    crash: watch::Sender<bool>,
    task: JoinHandle<Result<ShutdownOutcome, DaemonError>>,
    diagnostics: DaemonTaskDiagnostics,
}

/// Lifecycle channels allocated before the owner task is scheduled.
///
/// The binary liveness path uses this split to arm the kernel watcher first,
/// so an EOF/process signal already present at argument adoption is committed
/// to the shutdown journal before startup can touch the profile.
struct PreparedDaemonTask {
    states: StatePublisher,
    readiness: Readiness,
    shutdown: ShutdownHandle,
    shutdown_receiver: watch::Receiver<ShutdownRequest>,
    shutdown_observer: ShutdownObserver,
    crash: watch::Sender<bool>,
    crash_receiver: watch::Receiver<bool>,
    completion: Arc<DaemonTaskCompletion>,
}

struct DaemonTaskControl {
    shutdown_handle: ShutdownHandle,
    shutdown: watch::Receiver<ShutdownRequest>,
    shutdown_observer: ShutdownObserver,
    crash: watch::Receiver<bool>,
    diagnostics: Arc<DaemonTaskCompletion>,
}

impl PreparedDaemonTask {
    fn new() -> Self {
        let (states, readiness) = StatePublisher::channel();
        let (shutdown, shutdown_receiver, shutdown_observer) = ShutdownHandle::channel();
        let (crash, crash_receiver) = watch::channel(false);
        Self {
            states,
            readiness,
            shutdown,
            shutdown_receiver,
            shutdown_observer,
            crash,
            crash_receiver,
            completion: Arc::new(DaemonTaskCompletion::default()),
        }
    }

    fn shutdown_handle(&self) -> ShutdownHandle {
        self.shutdown.clone()
    }

    fn spawn(self, config: DaemonConfig, dependencies: DaemonDependencies) -> DaemonTask {
        let diagnostics = DaemonTaskDiagnostics {
            readiness: self.readiness.clone(),
            completion: Arc::clone(&self.completion),
        };
        let task_diagnostics = Arc::clone(&self.completion);
        let task_shutdown = self.shutdown.clone();
        let completion_guard = DaemonTaskCompletionGuard {
            completion: self.completion,
        };
        let task = tokio::spawn(async move {
            let result = run_owner(
                config,
                dependencies,
                self.states,
                DaemonTaskControl {
                    shutdown_handle: task_shutdown,
                    shutdown: self.shutdown_receiver,
                    shutdown_observer: self.shutdown_observer,
                    crash: self.crash_receiver,
                    diagnostics: task_diagnostics,
                },
            )
            .await;
            completion_guard.record(format!("{result:?}"));
            result
        });
        DaemonTask {
            readiness: self.readiness,
            shutdown: self.shutdown,
            crash: self.crash,
            task,
            diagnostics,
        }
    }
}

/// Cloneable, read-only health probe for an in-process daemon task.
///
/// Black-box integration tests use this instead of pretending the library
/// daemon has a child-process exit status or separately capturable stdio.
#[derive(Clone)]
pub struct DaemonTaskDiagnostics {
    readiness: Readiness,
    completion: Arc<DaemonTaskCompletion>,
}

/// One non-blocking daemon-task health snapshot.
#[derive(Debug, Clone)]
pub struct DaemonTaskDiagnosticSnapshot {
    /// Last lifecycle state published by the daemon owner.
    pub state: DaemonState,
    /// Whether the owner task returned, panicked, or was aborted.
    pub finished: bool,
    /// Bounded typed result, or the panic/abort marker installed by the guard.
    pub outcome: Option<String>,
    /// Connections admitted by the daemon since this task started.
    pub connection_admissions: u64,
}

#[derive(Default)]
struct DaemonTaskCompletion {
    finished: AtomicBool,
    outcome: StdMutex<Option<String>>,
    connection_admissions: Arc<AtomicU64>,
}

struct DaemonTaskCompletionGuard {
    completion: Arc<DaemonTaskCompletion>,
}

impl DaemonTaskCompletionGuard {
    fn record(&self, mut outcome: String) {
        const MAX_OUTCOME_BYTES: usize = 4 * 1024;
        if outcome.len() > MAX_OUTCOME_BYTES {
            let mut end = MAX_OUTCOME_BYTES.saturating_sub('…'.len_utf8());
            while !outcome.is_char_boundary(end) {
                end = end.saturating_sub(1);
            }
            outcome.truncate(end);
            outcome.push('…');
        }
        let mut slot = self
            .completion
            .outcome
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *slot = Some(outcome);
    }
}

impl Drop for DaemonTaskCompletionGuard {
    fn drop(&mut self) {
        {
            let mut slot = self
                .completion
                .outcome
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if slot.is_none() {
                *slot =
                    Some("daemon owner task panicked or was aborted before a typed outcome".into());
            }
        }
        self.completion.finished.store(true, Ordering::Release);
    }
}

impl DaemonTaskDiagnostics {
    #[must_use]
    pub fn snapshot(&self) -> DaemonTaskDiagnosticSnapshot {
        let finished = self.completion.finished.load(Ordering::Acquire);
        let outcome = self
            .completion
            .outcome
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        DaemonTaskDiagnosticSnapshot {
            state: self.readiness.current(),
            finished,
            outcome,
            connection_admissions: self
                .completion
                .connection_admissions
                .load(Ordering::Relaxed),
        }
    }
}

/// One-session-at-a-time fan-out for the pre-Ready journal scan. Pages feed
/// hook state and the native-pipe projector immediately; decoded envelopes
/// are dropped before the next bounded page is read.
struct StartupHydration {
    store: SqliteStoreHandle,
    hooks: HookStartupHydrator,
    pipe_native: Arc<PipeNativeWriter>,
    pipe_session: Option<PipeBootSession>,
}

impl StartupHydration {
    async fn prepare(store: &SqliteStoreHandle) -> Result<Self, HaiderError> {
        Ok(Self::with_hooks(
            store,
            HookStartupHydrator::prepare(store).await?,
        ))
    }

    fn with_hooks(store: &SqliteStoreHandle, hooks: HookStartupHydrator) -> Self {
        Self {
            store: store.clone(),
            hooks,
            pipe_native: Arc::new(PipeNativeWriter::new(store.root())),
            pipe_session: None,
        }
    }

    fn into_parts(self) -> (HookStartupHydrator, Arc<PipeNativeWriter>) {
        (self.hooks, self.pipe_native)
    }
}

#[cfg(test)]
pub(crate) async fn finish_hook_hydration_for_test(
    store: &SqliteStoreHandle,
    hooks: HookStartupHydrator,
) -> Result<HookStartupHydrator, HaiderError> {
    let mut startup = StartupHydration::with_hooks(store, hooks);
    recover_interrupted_turns_report_with_visitor(
        store,
        &DeviceId::new("hook-test-shared-hydration"),
        &mut startup,
    )
    .await?;
    Ok(startup.into_parts().0)
}

#[async_trait::async_trait]
impl StartupJournalVisitor for StartupHydration {
    async fn start_session(&mut self, session_id: &SessionId) -> Result<u64, HaiderError> {
        debug_assert!(self.pipe_session.is_none());
        let hook_cursor = self.hooks.scan_start(session_id);
        let pipe_cursor = match self
            .pipe_native
            .begin_boot_session(&self.store, session_id)
            .await
        {
            Ok(session) => {
                let cursor = session.scan_start();
                self.pipe_session = Some(session);
                cursor
            }
            Err(error) => {
                tracing::warn!(
                    session_id = %session_id,
                    %error,
                    "boot native pipe cursor inspection failed; requesting full shared replay"
                );
                0
            }
        };
        Ok(hook_cursor.min(pipe_cursor))
    }

    async fn visit_page(
        &mut self,
        session_id: &SessionId,
        page: &[RawEnvelope],
    ) -> Result<(), HaiderError> {
        self.hooks.fold_page(session_id, page);
        if let Some(pipe_session) = &mut self.pipe_session
            && let Err(error) = pipe_session.fold_page(page).await
        {
            self.pipe_native.invalidate(session_id);
            self.pipe_session = None;
            tracing::warn!(
                session_id = %session_id,
                %error,
                "boot native pipe page fold failed; journal remains authoritative"
            );
        }
        Ok(())
    }

    async fn finish_session(
        &mut self,
        store: &SqliteStoreHandle,
        session_id: &SessionId,
    ) -> Result<(), HaiderError> {
        // Turn recovery can append terminal facts after the main scan. Read
        // that suffix once and fan each bounded page to both remaining
        // consumers before either publishes its startup state.
        loop {
            let mut cursor = self.hooks.scan_start(session_id);
            if let Some(pipe_session) = &self.pipe_session {
                cursor = cursor.min(pipe_session.through_seq());
            }
            let page = store
                .read_reducer_page_with_boundary(
                    session_id,
                    cursor,
                    STARTUP_JOURNAL_PAGE_ENVELOPES,
                    STARTUP_JOURNAL_PAGE_BYTES,
                    STARTUP_VISITOR_PAYLOAD_KINDS,
                )
                .await?;
            if page.envelopes.is_empty() {
                if let Some((through_seq, boundary_event_id)) = page.observed_head {
                    self.hooks
                        .advance_through(session_id, through_seq, &boundary_event_id);
                    if let Some(pipe_session) = &mut self.pipe_session {
                        pipe_session.advance_through(through_seq);
                    }
                }
                break;
            }
            self.visit_page(session_id, &page.envelopes).await?;
        }
        if let Some(pipe_session) = self.pipe_session.take() {
            if let Err(error) = self
                .pipe_native
                .finish_boot_session(session_id, pipe_session)
                .await
            {
                self.pipe_native.invalidate(session_id);
                tracing::warn!(
                    session_id = %session_id,
                    %error,
                    "boot native pipe sidecar reconciliation failed; journal remains authoritative"
                );
            } else {
                self.pipe_native.release_clean(session_id);
            }
        }
        Ok(())
    }
}

impl DaemonTask {
    /// Phase observer — poll this instead of sleeping (R4 daemon half).
    pub fn readiness(&self) -> Readiness {
        self.readiness.clone()
    }

    /// Shutdown control: first request drains, later requests force (R17).
    pub fn shutdown_handle(&self) -> ShutdownHandle {
        self.shutdown.clone()
    }

    /// Non-consuming task health used by failure diagnostics.
    #[must_use]
    pub fn diagnostics(&self) -> DaemonTaskDiagnostics {
        self.diagnostics.clone()
    }

    /// Waits for the daemon to finish. `Err` values are the daemon's typed
    /// failures (including the loser-side `AlreadyRunning`).
    pub async fn join(self) -> Result<ShutdownOutcome, DaemonError> {
        self.task.await.map_err(|error| DaemonError::Task {
            message: format!("daemon owner task failed: {error}"),
        })?
    }

    /// Abruptly aborts the owner task without running the drain barrier.
    ///
    /// This is the in-process equivalent of process death used by restart
    /// recovery tests. Owned manager/supervisor drops abort their children;
    /// no graceful terminalization is attempted.
    pub async fn crash(self) {
        self.crash.send_replace(true);
        let _ = self.task.await;
    }
}

/// Starts one owned daemon task and returns observable lifecycle controls.
pub fn spawn(config: DaemonConfig) -> DaemonTask {
    spawn_with_dependencies(config, DaemonDependencies::default())
}

/// Starts the production runtime with injectable provider/tool factories.
pub fn spawn_with_dependencies(
    config: DaemonConfig,
    dependencies: DaemonDependencies,
) -> DaemonTask {
    PreparedDaemonTask::new().spawn(config, dependencies)
}

/// Runs until Unix termination signals stop the daemon.
///
/// The first SIGINT/SIGTERM starts the drain barrier; the second selects the
/// forced path. Crash/forced recovery belongs to the next daemon generation.
pub async fn run_with_signals(config: DaemonConfig) -> Result<ShutdownOutcome, DaemonError> {
    run_with_signals_and_dependencies(config, DaemonDependencies::default()).await
}

/// [`run_with_signals`] with injectable provider/tool factories — the
/// binary-level twin of [`spawn_with_dependencies`].
///
/// The only production caller passes [`DaemonDependencies::default`]; the
/// injectable form exists so an END-TO-END probe can drive the REAL daemon
/// binary (real socket, real store, real hub) against a deterministic
/// provider, with no network and no credentials.
pub async fn run_with_signals_and_dependencies(
    config: DaemonConfig,
    dependencies: DaemonDependencies,
) -> Result<ShutdownOutcome, DaemonError> {
    run_with_signals_and_dependencies_and_readiness(config, dependencies, None).await
}

/// Runs the signal-owned daemon and emits an optional launcher notification
/// on the same lifecycle edge exposed by [`DaemonTask::readiness`].
pub async fn run_with_signals_and_dependencies_and_readiness(
    config: DaemonConfig,
    dependencies: DaemonDependencies,
    launcher_readiness: Option<haider_platform::DaemonReadyNotifier>,
) -> Result<ShutdownOutcome, DaemonError> {
    run_with_signals_and_dependencies_and_readiness_and_liveness(
        config,
        dependencies,
        launcher_readiness,
        None,
    )
    .await
}

/// Runs the signal-owned daemon with optional startup readiness and ephemeral
/// launcher-liveness channels.
pub async fn run_with_signals_and_dependencies_and_readiness_and_liveness(
    config: DaemonConfig,
    dependencies: DaemonDependencies,
    mut launcher_readiness: Option<haider_platform::DaemonReadyNotifier>,
    launcher_liveness: Option<haider_platform::DaemonLivenessWatcher>,
) -> Result<ShutdownOutcome, DaemonError> {
    let mut signals =
        haider_platform::ShutdownSignals::new().map_err(|error| DaemonError::Task {
            message: format!("cannot install {} handler: {error}", error.signal()),
        })?;
    let prepared = PreparedDaemonTask::new();
    let shutdown = prepared.shutdown_handle();
    let (mut liveness_task, liveness_armed) = launcher_liveness.map_or_else(
        || (None, None),
        |watcher| {
            let (armed_sender, armed_receiver) = tokio::sync::oneshot::channel();
            let shutdown = shutdown.clone();
            let task = tokio::spawn(async move {
                let mut wait = Box::pin(watcher.wait());
                let initial =
                    std::future::poll_fn(|context| Poll::Ready(wait.as_mut().poll(context))).await;
                match initial {
                    Poll::Ready(outcome) => {
                        record_launcher_liveness(outcome, &shutdown);
                        let _ = armed_sender.send(());
                    }
                    Poll::Pending => {
                        let _ = armed_sender.send(());
                        record_launcher_liveness(wait.await, &shutdown);
                    }
                }
            });
            (Some(task), Some(armed_receiver))
        },
    );
    if let Some(armed) = liveness_armed {
        // The watcher has either observed an already-dead launcher or has
        // registered its kernel wait before the owner task may start.
        match armed.await {
            Ok(()) => {
                eprintln!(
                    "haiderd: ephemeral-lifecycle event=guard_armed result=registered_or_already_signaled"
                );
                tracing::info!(
                    result = "registered_or_already_signaled",
                    "ephemeral launcher guard armed"
                );
            }
            Err(error) => {
                eprintln!(
                    "haiderd: ephemeral-lifecycle event=guard_armed result=failed error={error}"
                );
                tracing::warn!(%error, "ephemeral launcher guard failed to arm");
            }
        }
    }
    let task = prepared.spawn(config, dependencies);
    let mut readiness = task.readiness();
    let mut joined: Pin<Box<dyn Future<Output = Result<ShutdownOutcome, DaemonError>> + Send>> =
        Box::pin(task.join());
    notify_launcher_if_ready(&readiness, &mut launcher_readiness);
    loop {
        tokio::select! {
            result = &mut joined => {
                if let Some(watcher) = liveness_task.take() {
                    watcher.abort();
                    let _ = watcher.await;
                }
                return result;
            },
            state = readiness.changed(), if launcher_readiness.is_some() => {
                match state {
                    Some(DaemonState::Ready) => {
                        notify_launcher_if_ready(&readiness, &mut launcher_readiness);
                    }
                    Some(DaemonState::Failed { .. } | DaemonState::Stopped) | None => {
                        launcher_readiness = None;
                    }
                    Some(_) => {}
                }
            }
            signal = haider_platform::shutdown_signal(&mut signals) => {
                if let Some(signal) = signal {
                    shutdown.request(signal.reason());
                }
            }
        }
    }
}

fn record_launcher_liveness(outcome: std::io::Result<()>, shutdown: &ShutdownHandle) {
    match outcome {
        Ok(()) => {
            eprintln!(
                "haiderd: ephemeral-lifecycle event=launcher_liveness result=launcher_exited"
            );
            tracing::info!("ephemeral launcher liveness ended");
        }
        Err(error) => {
            // This watcher exists only for an ephemeral launch. Once its kernel
            // proof becomes unusable, fail closed into the same idle barrier
            // instead of risking an unsupervised daemon.
            eprintln!(
                "haiderd: ephemeral-lifecycle event=launcher_liveness result=error error={error}"
            );
            tracing::warn!(%error, "launcher-liveness watch failed");
        }
    }
    if shutdown.request_when_idle(ShutdownReason::ClientVanished) {
        eprintln!(
            "haiderd: ephemeral-lifecycle event=idle_shutdown_armed reason=launcher_vanished"
        );
        tracing::info!(
            reason = ?ShutdownReason::ClientVanished,
            "ephemeral daemon observed launcher process exit"
        );
    } else {
        eprintln!(
            "haiderd: ephemeral-lifecycle event=idle_shutdown_not_armed reason=shutdown_already_requested"
        );
        tracing::info!("ephemeral idle shutdown was already superseded");
    }
}

fn notify_launcher_if_ready(
    readiness: &Readiness,
    launcher_readiness: &mut Option<haider_platform::DaemonReadyNotifier>,
) {
    if !matches!(readiness.current(), DaemonState::Ready) {
        return;
    }
    if let Some(notification) = launcher_readiness.take()
        && let Err(error) = notification.notify()
    {
        tracing::warn!(%error, "could not notify daemon launcher that the listener is ready");
    }
}

async fn run_owner(
    config: DaemonConfig,
    dependencies: DaemonDependencies,
    states: StatePublisher,
    control: DaemonTaskControl,
) -> Result<ShutdownOutcome, DaemonError> {
    let result = run_inner(&config, dependencies, &states, control).await;
    if let Err(error) = &result {
        states.publish(DaemonState::Failed {
            message: error.to_string(),
        });
    }
    result
}

async fn run_inner(
    config: &DaemonConfig,
    dependencies: DaemonDependencies,
    states: &StatePublisher,
    control: DaemonTaskControl,
) -> Result<ShutdownOutcome, DaemonError> {
    let DaemonTaskControl {
        shutdown_handle,
        mut shutdown,
        shutdown_observer,
        mut crash,
        diagnostics,
    } = control;
    config
        .validate()
        .map_err(|message| DaemonError::InvalidConfig { message })?;
    endpoint::validate_budget(config)?;
    if !matches!(*shutdown.borrow(), ShutdownRequest::None) {
        let request = shutdown.borrow().clone();
        return shutdown_without_store(config, states, request, &shutdown);
    }
    // The lockdown ceiling is machine-user global, not profile-scoped. Open
    // and reconcile it before acquiring any per-profile resources so a
    // typed startup failure cannot strand a lease, store, or actor.
    crate::lockdown::initialize_global(config.lockdown_root_override.as_deref()).map_err(
        |error| DaemonError::Lockdown {
            message: error.to_string(),
        },
    )?;
    let endpoint_path = config.endpoint_path();
    // R1: the profile lifetime lock is acquired before any socket cleanup or
    // store open, and it is the only singleton authority. A held lock means
    // an incumbent daemon exists — exit typed, with diagnostics only.
    let lease = match SqliteStoreHandle::acquire_profile(&config.store_dir).await {
        Ok(lease) => lease,
        Err(error) if error.code == ErrorCode::StoreLocked => {
            return Err(DaemonError::AlreadyRunning {
                diagnostics: incumbent_diagnostics(config, &endpoint_path),
            });
        }
        Err(error) => return Err(error.into()),
    };
    haider_platform::publish_active_daemon_log(&config.store_dir).map_err(|error| {
        DaemonError::Task {
            message: format!("cannot publish active daemon log: {error}"),
        }
    })?;
    if !matches!(*shutdown.borrow(), ShutdownRequest::None) {
        let request = shutdown.borrow().clone();
        drop(lease);
        return shutdown_without_store(config, states, request, &shutdown);
    }
    let mut runtime_directory = endpoint::RuntimeDirectory::prepare(&config.runtime_dir)?;
    if !matches!(*shutdown.borrow(), ShutdownRequest::None) {
        let request = shutdown.borrow().clone();
        runtime_directory.cleanup()?;
        drop(lease);
        return shutdown_without_store(config, states, request, &shutdown);
    }

    // R16 ready gate: open store under the lock -> durable generation bump ->
    // reconcile every dispatched-without-terminal effect. Only after all of
    // this may a listener bind or Ready be advertised.
    states.publish(DaemonState::Recovering);
    let store = SqliteStoreHandle::open_locked(lease).await?;
    if let Err(error) = store.initialize_usage_history().await {
        let _ = store.close().await;
        return Err(error.into());
    }
    let (effect_diagnostics, prior_unexpected_exits) =
        EffectDiagnostics::open(config.store_dir.clone())
            .await
            .map_err(|error| DaemonError::Task {
                message: format!("cannot open effect diagnostic journal: {error}"),
            })?;
    for evidence in &prior_unexpected_exits {
        eprintln!(
            "haiderd: prior unexpected exit evidence: build={} build_uuid={} pid={} \
             process_started_unix_ms={} thread={} effect={} session={} run={} tool={} \
             workspace_root_digest={} args_digest={} effect_started_unix_ms={}",
            evidence.build_version,
            evidence.build_uuid,
            evidence.pid,
            evidence.process_started_unix_ms,
            evidence.thread_name,
            evidence.effect_id,
            evidence.session_id,
            evidence.run_id,
            evidence.tool_name,
            evidence.workspace_root_digest,
            evidence.args_digest,
            evidence.started_unix_ms,
        );
    }
    effect_diagnostics
        .record_surfaced(prior_unexpected_exits)
        .await
        .map_err(|error| DaemonError::Task {
            message: format!("cannot persist surfaced effect diagnostics: {error}"),
        })?;
    let instance_id = random_instance_id()?;
    let daemon_generation = match store.advance_daemon_generation().await {
        Ok(generation) => generation,
        Err(error) => {
            let _ = store.close().await;
            return Err(error.into());
        }
    };
    let device_id = DeviceId::new(format!("daemon-{instance_id}"));
    let effect_recovery_started = Instant::now();
    match reconcile_before_ready(&store, &device_id, &mut shutdown).await {
        Some(Ok(_)) => {}
        Some(Err(error)) => {
            let _ = store.close().await;
            return Err(error.into());
        }
        None => {
            let request = shutdown.borrow().clone();
            return shutdown_before_listener(
                config,
                states,
                store,
                runtime_directory,
                request,
                &mut shutdown,
            )
            .await;
        }
    }
    tracing::trace!(
        target: "haider.recovery",
        phase = "effects",
        operation_micros = effect_recovery_started.elapsed().as_micros(),
        "pre-ready recovery phase completed"
    );
    if !matches!(*shutdown.borrow(), ShutdownRequest::None) {
        let request = shutdown.borrow().clone();
        return shutdown_before_listener(
            config,
            states,
            store,
            runtime_directory,
            request,
            &mut shutdown,
        )
        .await;
    }
    let mut startup_hydration = match StartupHydration::prepare(&store).await {
        Ok(hydration) => hydration,
        Err(error) => {
            let _ = store.close().await;
            return Err(error.into());
        }
    };
    if let Some(request) = startup_shutdown_request(&shutdown) {
        return shutdown_before_listener(
            config,
            states,
            store,
            runtime_directory,
            request,
            &mut shutdown,
        )
        .await;
    }
    let turn_recovery_started = Instant::now();
    let turn_recovery = match recover_interrupted_turns_report_with_visitor(
        &store,
        &device_id,
        &mut startup_hydration,
    )
    .await
    {
        Ok(recovery) => recovery,
        Err(error) => {
            let _ = store.close().await;
            return Err(error.into());
        }
    };
    tracing::trace!(
        target: "haider.recovery",
        phase = "turns",
        recovered_work = turn_recovery.work.len(),
        touched_sessions = turn_recovery.touched_sessions.len(),
        operation_micros = turn_recovery_started.elapsed().as_micros(),
        "pre-ready recovery phase completed"
    );
    if let Some(request) = startup_shutdown_request(&shutdown) {
        return shutdown_before_listener(
            config,
            states,
            store,
            runtime_directory,
            request,
            &mut shutdown,
        )
        .await;
    }
    // W3c2 R10 startup phase: load the descriptor store and reconcile
    // pending/committed LOGIN receipts against vault + descriptor truth
    // before anything can observe Ready (run_inner's receipt-reconciliation
    // phase — W3c1 receipts never persist `pending`; login's can).
    let accounts_started = tokio::time::Instant::now();
    let creatable_providers = dependencies.provider_factory.creatable_providers();
    let accounts_runtime = match crate::accounts::AccountsRuntime::initialize(
        &store,
        &dependencies.accounts,
        &config.store_dir,
        &config.profile_id,
        &instance_id,
        &config.default_model,
        &creatable_providers,
        config.discovery_disabled,
    )
    .await
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = store.close().await;
            return Err(error.into());
        }
    };
    tracing::trace!(
        target: "haider.recovery",
        phase = "login_receipts",
        operation_micros = accounts_started.elapsed().as_micros(),
        "pre-ready recovery phase completed"
    );

    // Owner defaults (2026-08-20): the Loom registry seeds its two default
    // agent types before Ready — absent-only, so user revisions are never
    // clobbered. A seed failure is fatal like any other pre-Ready phase.
    if let Err(error) = crate::loom_seed::seed_loom_registry(&store).await {
        let _ = store.close().await;
        return Err(error.into());
    }
    let (hook_hydration, pipe_native) = startup_hydration.into_parts();
    let hub = SessionHub::new_with_pipe_native(store.clone(), config.session_hub, pipe_native)
        .map_err(DaemonError::from)?;
    let recovered_work = turn_recovery.work;
    let (hook_service, hook_engine) = crate::hooks::HookEngine::start_hydrated(
        config.store_dir.clone(),
        store.clone(),
        hub.clone(),
        hook_hydration,
    )
    .await
    .map_err(DaemonError::from)?;
    hub.install_hooks(hook_service).map_err(DaemonError::from)?;
    let mut hook_engine = Some(hook_engine);
    // D3-5 whitelist unification + the production factory swap: the ONE
    // provider authority is the dependency configuration. `Accounts` (the
    // default) resolves per logical turn from the daemon-owned account
    // snapshot + vault, so a committed login is picked up by the NEXT
    // logical turn; `"fake"` is creatable only under an injected test
    // configuration, never on the production wire path.
    let account_resilience = accounts_runtime.resilience.clone();
    let provider_factory: std::sync::Arc<dyn crate::worker::ProviderFactory> = match &dependencies
        .provider_factory
    {
        crate::worker::ProviderFactoryConfig::Accounts => match accounts_runtime.broker.clone() {
            Some(broker) => std::sync::Arc::new(
                crate::accounts::AccountsProviderFactory::with_broker_and_management(
                    std::sync::Arc::clone(&accounts_runtime.facade.snapshot),
                    accounts_runtime.facade.management.clone(),
                    accounts_runtime.vault.clone(),
                    std::sync::Arc::new(crate::accounts::ProductionAccountBuilder::default()),
                    broker,
                )
                .with_model_source(std::sync::Arc::clone(&accounts_runtime.model_source))
                .with_resilience(account_resilience.clone()),
            ),
            None => std::sync::Arc::new(
                crate::accounts::AccountsProviderFactory::new_with_management(
                    std::sync::Arc::clone(&accounts_runtime.facade.snapshot),
                    accounts_runtime.facade.management.clone(),
                    accounts_runtime.vault.clone(),
                    std::sync::Arc::new(crate::accounts::ProductionAccountBuilder::default()),
                )
                .with_model_source(std::sync::Arc::clone(&accounts_runtime.model_source))
                .with_resilience(account_resilience.clone()),
            ),
        },
        crate::worker::ProviderFactoryConfig::AccountsWith(builder) => {
            match accounts_runtime.broker.clone() {
                Some(broker) => std::sync::Arc::new(
                    crate::accounts::AccountsProviderFactory::with_broker(
                        std::sync::Arc::clone(&accounts_runtime.facade.snapshot),
                        accounts_runtime.vault.clone(),
                        std::sync::Arc::clone(builder),
                        broker,
                    )
                    .with_resilience(account_resilience.clone()),
                ),
                None => std::sync::Arc::new(
                    crate::accounts::AccountsProviderFactory::new(
                        std::sync::Arc::clone(&accounts_runtime.facade.snapshot),
                        accounts_runtime.vault.clone(),
                        std::sync::Arc::clone(builder),
                    )
                    .with_resilience(account_resilience.clone()),
                ),
            }
        }
        crate::worker::ProviderFactoryConfig::Injected { factory, .. } => {
            std::sync::Arc::clone(factory)
        }
    };
    hub.install_creatable_providers(creatable_providers)
        .map_err(DaemonError::from)?;
    hub.install_loom_author_provider(std::sync::Arc::clone(&provider_factory))
        .map_err(DaemonError::from)?;
    // U1: the read-only `usage.report` service shares the account snapshot
    // and (when the vault runs) the SAME credential broker as provider
    // construction, so meter fetches ride the broker's refresh single-flight
    // instead of racing it.
    hub.install_usage_report(std::sync::Arc::new(
        crate::usage_report::UsageReportService::new(
            std::sync::Arc::clone(&accounts_runtime.facade.snapshot),
            accounts_runtime.broker.clone().map(|broker| {
                std::sync::Arc::new(broker)
                    as std::sync::Arc<dyn crate::usage_report::MeterTokenSource>
            }),
            std::sync::Arc::new(crate::usage_report::ReqwestUsageMeterHttp::new()),
        ),
    ))
    .map_err(DaemonError::from)?;
    // W-B (decision 3): the client `web_search` tool on responses-lite pairs
    // executes through the SAME credential broker as turns. Without a vault
    // there is no broker and therefore no subscription credential — the tool
    // then answers with a typed "unavailable" result instead of pretending.
    let web_search: Option<std::sync::Arc<dyn crate::worker::WebSearchExecutor>> =
        accounts_runtime.broker.clone().map(|broker| {
            std::sync::Arc::new(crate::web_search::SubscriptionWebSearch::new(
                std::sync::Arc::new(broker)
                    as std::sync::Arc<dyn crate::web_search::WebSearchCredentials>,
                std::sync::Arc::new(crate::web_search::ReqwestWebSearchHttp::new()),
            )) as std::sync::Arc<dyn crate::worker::WebSearchExecutor>
        });
    let worker_dependencies = crate::worker::WorkerDependencies {
        provider_factory,
        tool_factory: std::sync::Arc::clone(&dependencies.tool_factory),
        delegation: None,
        web_search,
        diagnostics: Some(effect_diagnostics),
    };
    let worker_manager = WorkerManager::start(
        hub.clone(),
        worker_dependencies,
        config.inject_worker_manager_shutdown_error,
    );
    let worker_handle = worker_manager.handle();
    hub.install_worker_manager(worker_handle.clone())
        .map_err(DaemonError::from)?;
    let crate::accounts::AccountsRuntime {
        facade: accounts_facade,
        actor: mut account_actor,
        model_source: _,
        vault: _,
        broker: credential_broker,
        resilience: _,
    } = accounts_runtime;
    let oauth_coordinator = accounts_facade.oauth.clone();
    let account_management = accounts_facade.management.clone();
    if let Some(request) = startup_shutdown_request(&shutdown) {
        return shutdown_started_runtime_before_listener(
            config,
            states,
            StartupShutdownResources {
                store,
                runtime_directory,
                worker_manager,
                credential_broker,
                oauth_coordinator,
                account_actor,
                hook_engine,
                hub,
                mobile_server: None,
            },
            request,
            &mut shutdown,
        )
        .await;
    }
    // The monitor registry must finish durable boot adoption before any
    // external source can publish. Otherwise an authenticated startup event
    // could outrun an existing watch or its timeout fence.
    hub.wait_for_monitor_ready().await;
    if let Some(request) = startup_shutdown_request(&shutdown) {
        return shutdown_started_runtime_before_listener(
            config,
            states,
            StartupShutdownResources {
                store,
                runtime_directory,
                worker_manager,
                credential_broker,
                oauth_coordinator,
                account_actor,
                hook_engine,
                hub,
                mobile_server: None,
            },
            request,
            &mut shutdown,
        )
        .await;
    }
    hub.install_accounts(accounts_facade)
        .map_err(DaemonError::from)?;
    for work in recovered_work {
        let result = match work {
            RecoveredWork::Queued(accepted) => worker_handle.recover_queued(accepted).await,
            RecoveredWork::Retry(accepted) => worker_handle.recover_retry(accepted).await,
            RecoveredWork::Checkpoint(recovered) => {
                worker_handle
                    .recover_checkpoint(
                        recovered.accepted,
                        recovered.checkpoint,
                        recovered.committed_answer,
                    )
                    .await
            }
            RecoveredWork::PartialStream(recovered) => {
                worker_handle
                    .recover_partial_stream(
                        recovered.accepted,
                        recovered.checkpoint,
                        recovered.committed_answer,
                    )
                    .await
            }
            RecoveredWork::ChildWait(recovered) => {
                worker_handle
                    .recover_child_wait(recovered.accepted, recovered.checkpoint)
                    .await
            }
        };
        if let Err(error) = result {
            let _ = worker_manager.shutdown().await;
            if let Some(broker) = &credential_broker {
                broker.abort_and_join().await;
            }
            if let Some(oauth) = &oauth_coordinator {
                oauth.abort_and_join().await;
            }
            if let Some(actor) = account_actor.as_mut() {
                actor.force_and_join().await;
            }
            if let Some(engine) = hook_engine.take() {
                engine.shutdown().await;
            }
            let _ = hub.shutdown().await;
            let _ = store.close().await;
            return Err(error.into());
        }
        if let Some(request) = startup_shutdown_request(&shutdown) {
            return shutdown_started_runtime_before_listener(
                config,
                states,
                StartupShutdownResources {
                    store,
                    runtime_directory,
                    worker_manager,
                    credential_broker,
                    oauth_coordinator,
                    account_actor,
                    hook_engine,
                    hub,
                    mobile_server: None,
                },
                request,
                &mut shutdown,
            )
            .await;
        }
    }
    // Peer mailboxes start only after ordinary turn recovery has handed every
    // previously accepted run back to the worker. This makes an Accepted
    // mailbox record a recovery observation, never a duplicate admission.
    let peer_service = match crate::peer::PeerService::start(config.runtime_dir.clone(), &hub).await
    {
        Ok(service) => service,
        Err(error) => {
            let _ = worker_manager.shutdown().await;
            if let Some(broker) = &credential_broker {
                broker.abort_and_join().await;
            }
            if let Some(oauth) = &oauth_coordinator {
                oauth.abort_and_join().await;
            }
            if let Some(actor) = account_actor.as_mut() {
                actor.force_and_join().await;
            }
            if let Some(engine) = hook_engine.take() {
                engine.shutdown().await;
            }
            let _ = hub.shutdown().await;
            let _ = store.close().await;
            return Err(DaemonError::Task {
                message: format!("peer messaging startup failed: {error}"),
            });
        }
    };
    if let Err(error) = hub.install_peer_service(peer_service) {
        let _ = worker_manager.shutdown().await;
        if let Some(broker) = &credential_broker {
            broker.abort_and_join().await;
        }
        if let Some(oauth) = &oauth_coordinator {
            oauth.abort_and_join().await;
        }
        if let Some(actor) = account_actor.as_mut() {
            actor.force_and_join().await;
        }
        if let Some(engine) = hook_engine.take() {
            engine.shutdown().await;
        }
        let _ = hub.shutdown().await;
        let _ = store.close().await;
        return Err(DaemonError::Task {
            message: format!("peer messaging installation failed: {error}"),
        });
    }
    // Install both monitor seams before the accept task exists. From the
    // first authenticated APK frame onward, every valid SMS therefore has an
    // active source subscriber and every report has the canonical chat sink.
    let mut mobile_server = match crate::mobile_transport::start_if_enabled(
        config,
        hub.clone(),
        config.default_model.clone(),
        instance_id.clone(),
    )
    .await
    {
        Ok(server) => server,
        Err(error) => {
            let _ = worker_manager.shutdown().await;
            if let Some(broker) = &credential_broker {
                broker.abort_and_join().await;
            }
            if let Some(oauth) = &oauth_coordinator {
                oauth.abort_and_join().await;
            }
            if let Some(actor) = account_actor.as_mut() {
                actor.force_and_join().await;
            }
            if let Some(engine) = hook_engine.take() {
                engine.shutdown().await;
            }
            let _ = hub.shutdown().await;
            let _ = store.close().await;
            return Err(error);
        }
    };
    if let Some(request) = startup_shutdown_request(&shutdown) {
        return shutdown_started_runtime_before_listener(
            config,
            states,
            StartupShutdownResources {
                store,
                runtime_directory,
                worker_manager,
                credential_broker,
                oauth_coordinator,
                account_actor,
                hook_engine,
                hub,
                mobile_server,
            },
            request,
            &mut shutdown,
        )
        .await;
    }
    // Every boot scan, adoption, recovered-work handoff, and optional
    // transport startup has returned, while no endpoint exists for external
    // requests. The store adapter serializes this operation with every other
    // store call, and the connection mutex excludes any live SQLite statement
    // or transaction. Keep the normal cache ceiling and prepared-statement
    // cache intact; discard only pages made cold by this boot.
    if let Err(error) = store.release_memory().await {
        tracing::warn!(%error, "SQLite boot-page release failed; continuing startup");
    }
    let mut endpoint = match endpoint::bind(config, runtime_directory).await {
        Ok(endpoint) => endpoint,
        Err(error) => {
            if let Some(server) = mobile_server.as_mut() {
                server.shutdown().await;
            }
            let _ = worker_manager.shutdown().await;
            if let Some(broker) = &credential_broker {
                broker.abort_and_join().await;
            }
            if let Some(oauth) = &oauth_coordinator {
                oauth.abort_and_join().await;
            }
            if let Some(actor) = account_actor.as_mut() {
                actor.force_and_join().await;
            }
            if let Some(engine) = hook_engine.take() {
                engine.shutdown().await;
            }
            let _ = hub.shutdown().await;
            let _ = store.flush().await;
            let _ = store.close().await;
            return Err(error);
        }
    };
    let mut haider_code_plan_poller = credential_broker.clone().map(|broker| {
        crate::haider_code_plan::HaiderCodePlanPoller::start_production(
            hub.clone(),
            account_management,
            broker,
        )
    });
    let (drain_sender, drain_receiver) = watch::channel(Option::<DrainNotice>::None);
    // Every connection hands its writer task here; the barrier below owns
    // aborting and JOINING them, so no child outlives teardown (R17).
    let (writer_sender, writer_receiver) = tokio::sync::mpsc::unbounded_channel();
    let context = ConnectionContext {
        profile_id: config.profile_id.clone(),
        instance_id: instance_id.clone(),
        daemon_generation,
        frame_limit: config.frame_limit,
        outbound_queue_capacity: config.outbound_queue_capacity,
        outbound_queued_bytes: config.outbound_queued_bytes,
        max_connections: config.max_connections,
        handshake_timeout: config.handshake_timeout,
        writers: writer_sender,
        owner_uid: endpoint.owner_uid(),
        hub: hub.clone(),
        shutdown: shutdown_handle,
        endpoint_path: endpoint.path().to_path_buf(),
    };
    // Ready is published under the shutdown transition mutex, so a first
    // signal that races this point either wins (no Ready, drain from
    // Recovering) or loses (Ready, then a normal drain).
    shutdown_observer.publish_ready_if_idle(states);

    let mut runtime = ConnectionRuntime::new(
        config.max_connections,
        writer_receiver,
        Arc::clone(&diagnostics.connection_admissions),
    );
    let (stop, listener_error) = runtime
        .accept_until_shutdown(
            &endpoint,
            &context,
            &drain_receiver,
            &mut shutdown,
            &mut crash,
        )
        .await;

    if matches!(&stop, RuntimeStop::Crash) {
        // In-process process-death seam: tear down ownership without sending
        // cancellation to workers and without appending terminal run events.
        // The next generation alone interprets the durable prefix.
        endpoint.close_listener();
        if let Some(server) = mobile_server.as_mut() {
            server.shutdown().await;
        }
        runtime.crash().await;
        worker_manager.crash().await;
        if let Some(poller) = haider_code_plan_poller.as_mut() {
            poller.abort_and_join().await;
        }
        if let Some(broker) = &credential_broker {
            broker.abort_and_join().await;
        }
        if let Some(oauth) = &oauth_coordinator {
            oauth.abort_and_join().await;
        }
        if let Some(actor) = account_actor {
            actor.crash();
        }
        drop(hook_engine.take());
        let _ = hub.shutdown().await;
        drop(context);
        drop(hub);
        drop(endpoint);
        store.close().await?;
        return Ok(ShutdownOutcome::Forced);
    }
    let RuntimeStop::Shutdown(request) = stop else {
        unreachable!("crash returned above")
    };

    // R17 drain barrier, in order: stop accepting, publish Draining,
    // broadcast ServerDraining to every connection, bounded completion,
    // flush, remove the exact owned socket, close the store (lock release)
    // LAST. One deadline bounds every abortable stage. A non-abortable account
    // vault write is the sole fail-safe exception: after the deadline, Stopped
    // and lock release remain withheld until that write is joined and its
    // rotated value is durably restored or failed closed.
    eprintln!(
        "haiderd: ephemeral-lifecycle event=cleanup step=listener_close outcome=starting path={}",
        endpoint.path().display()
    );
    #[cfg(windows)]
    eprintln!(
        "haiderd: ephemeral-lifecycle event=windows_pipe_close_order step=listener_instance before=connection_drain"
    );
    endpoint.close_listener();
    eprintln!(
        "haiderd: ephemeral-lifecycle event=cleanup step=listener_close outcome=closed path={}",
        endpoint.path().display()
    );
    tracing::info!(
        path = %endpoint.path().display(),
        "daemon listener closed before connection drain"
    );
    let (reason, mut forced) = match request {
        ShutdownRequest::Graceful { reason } | ShutdownRequest::GracefulWhenIdle { reason } => {
            (reason.to_string(), false)
        }
        ShutdownRequest::Forced { reason } => (reason.to_string(), true),
        // Unreachable: the loop above only breaks with a real request.
        ShutdownRequest::None => ("internal shutdown".into(), true),
    };
    if let Some(server) = mobile_server.as_mut() {
        server.shutdown().await;
    }
    let barrier_deadline = tokio::time::Instant::now() + config.drain_timeout;
    let deadline_unix_ms = unix_time_ms().saturating_add(duration_ms(config.drain_timeout));
    // R9 EXTERNAL ADMISSION GATE: this one flag closes every actor-CREATING
    // path at once — connection requests and menu answers (checked in
    // rpc.rs), new attachments, new worker leases — before Draining becomes
    // externally observable. Workers holding leases are NOT gated: their
    // final appends reach their existing actors (`SessionHub::existing_actor`
    // documents the asymmetry). The watch broadcast wakes connection tasks
    // on other executor workers.
    // Linearize manager admission first. Any successful try_send happened
    // under this gate and is FIFO-before Shutdown; any post-commit hint that
    // loses the gate is recovered by the manager's durable queued-run sweep.
    worker_handle.begin_draining();
    hub.begin_draining();
    states.publish(DaemonState::Draining {
        reason: reason.clone(),
        deadline_unix_ms,
    });

    // WORKER-AWARE §6.6 grace ORDER (R9 steps 3-5, extending W3b2.3 P1-3):
    // (a) workers settle FIRST — the manager cancels active turns, closes
    // effect brokers, terminalizes durable queued runs, and every terminal
    // append lands through the still-serving session actors; (b) then the
    // hub drains — remaining in-flight appends/CAS complete their persist
    // AND publish, and replay tasks stream those final committed envelopes
    // into the connection outboxes; (c) only then does the drain notice
    // fire. Reversing (a) and (b) would reject the workers' own terminal
    // `Cancelled`/effect/idle appends; firing (c) earlier would cancel
    // attachments while committed envelopes were still in flight to them.
    // The writer then puts `ServerDraining` on the wire at its next
    // complete-frame boundary and drains the already-queued checkpoint
    // envelopes under the same deadline (the ledgered W3b1 relaxation).
    let worker_shutdown =
        bounded_finalization(worker_manager.shutdown(), barrier_deadline, &mut shutdown).await;
    let worker_error = match worker_shutdown {
        Some(Ok(())) => None,
        Some(Err(error)) => {
            forced = true;
            Some(DaemonError::from(error))
        }
        None => {
            forced = true;
            None
        }
    };
    if let Some(poller) = haider_code_plan_poller.as_mut() {
        match bounded_finalization(poller.shutdown(), barrier_deadline, &mut shutdown).await {
            Some(true) => {}
            Some(false) | None => {
                forced = true;
                poller.abort_and_join().await;
            }
        }
    }
    // R10 drain: new account commands were already rejected by the hub's
    // draining flag; join the account actor (its in-flight login finishes or
    // the deadline forces it — pending receipts + reconciliation carry the
    // truth either way) under the SAME global deadline.
    if let Some(broker) = &credential_broker {
        match bounded_finalization(broker.shutdown(), barrier_deadline, &mut shutdown).await {
            Some(true) => {}
            Some(false) => forced = true,
            None => {
                forced = true;
                broker.abort_and_join().await;
            }
        }
    }
    if let Some(oauth) = &oauth_coordinator {
        match bounded_finalization(oauth.shutdown(), barrier_deadline, &mut shutdown).await {
            Some(true) => {}
            Some(false) => forced = true,
            None => {
                forced = true;
                oauth.abort_and_join().await;
            }
        }
    }
    if let Some(mut actor) = account_actor
        && bounded_finalization(actor.shutdown(), barrier_deadline, &mut shutdown)
            .await
            .is_none()
    {
        forced = true;
        // Unlike Tokio tasks, the actor's blocking vault persistence
        // cannot be aborted. Keep the profile lock and withhold Stopped
        // until the actor has observed the force fence, tombstoned any
        // rotated bundle, dropped its zeroizing bytes, and joined.
        actor.force_and_join().await;
    }
    if let Some(engine) = hook_engine.take()
        && bounded_finalization(engine.shutdown(), barrier_deadline, &mut shutdown)
            .await
            .is_none()
    {
        forced = true;
    }
    let hub_shutdown = bounded_finalization(hub.shutdown(), barrier_deadline, &mut shutdown).await;
    let hub_error = match hub_shutdown {
        Some(Ok(crate::SessionHubShutdownOutcome::Graceful)) => None,
        Some(Ok(crate::SessionHubShutdownOutcome::Forced)) => {
            forced = true;
            None
        }
        Some(Err(error)) => {
            forced = true;
            Some(DaemonError::from(error))
        }
        None => {
            forced = true;
            None
        }
    };
    drain_sender.send_replace(Some(DrainNotice {
        reason,
        instance_id,
        daemon_generation,
        deadline_unix_ms,
        deadline: barrier_deadline,
    }));
    let undelivered_notices = runtime
        .drain(&mut forced, barrier_deadline, &mut shutdown)
        .await;
    if undelivered_notices > 0 {
        forced = true;
    }
    let finalize_error = finalize(
        store,
        &mut endpoint,
        barrier_deadline,
        &mut shutdown,
        &mut forced,
    )
    .await;

    // Precedence: a listener failure is the daemon's own fault and outranks
    // whatever the barrier then reported. All errors are arbitrated only
    // after the full tail: in particular, no manager/hub failure may bypass
    // the account actor's transitive blocking-write join or release the
    // profile lease before finalization.
    if let Some(error) = listener_error
        .or(worker_error)
        .or(hub_error)
        .or(finalize_error)
    {
        return Err(error);
    }
    states.publish(DaemonState::Stopped);
    Ok(if forced {
        ShutdownOutcome::Forced
    } else {
        ShutdownOutcome::Graceful
    })
}

/// Runtime services that may already exist when launcher death arrives before
/// the listener is published. They are stopped in the same dependency order
/// as the ordinary drain before the store/runtime tail runs.
struct StartupShutdownResources {
    store: SqliteStoreHandle,
    runtime_directory: endpoint::RuntimeDirectory,
    worker_manager: WorkerManager,
    credential_broker: Option<crate::oauth::CredentialBroker>,
    oauth_coordinator: Option<crate::oauth::OAuthCoordinator>,
    account_actor: Option<crate::accounts::AccountActorHandle>,
    hook_engine: Option<crate::hooks::HookEngine>,
    hub: SessionHub,
    mobile_server: Option<crate::mobile_transport::MobileTransportServer>,
}

async fn shutdown_started_runtime_before_listener(
    config: &DaemonConfig,
    states: &StatePublisher,
    resources: StartupShutdownResources,
    request: ShutdownRequest,
    shutdown: &mut watch::Receiver<ShutdownRequest>,
) -> Result<ShutdownOutcome, DaemonError> {
    let StartupShutdownResources {
        store,
        runtime_directory,
        worker_manager,
        credential_broker,
        oauth_coordinator,
        mut account_actor,
        hook_engine,
        hub,
        mut mobile_server,
    } = resources;
    if let Some(server) = mobile_server.as_mut() {
        server.shutdown().await;
    }
    worker_manager.handle().begin_draining();
    hub.begin_draining();
    let _ = worker_manager.shutdown().await;
    if let Some(broker) = &credential_broker
        && !broker.shutdown().await
    {
        broker.abort_and_join().await;
    }
    if let Some(oauth) = &oauth_coordinator
        && !oauth.shutdown().await
    {
        oauth.abort_and_join().await;
    }
    if let Some(actor) = account_actor.as_mut() {
        actor.shutdown().await;
    }
    if let Some(engine) = hook_engine {
        engine.shutdown().await;
    }
    let _ = hub.shutdown().await;
    shutdown_before_listener(config, states, store, runtime_directory, request, shutdown).await
}

fn startup_shutdown_request(
    shutdown: &watch::Receiver<ShutdownRequest>,
) -> Option<ShutdownRequest> {
    match shutdown.borrow().clone() {
        ShutdownRequest::None => None,
        request => Some(request),
    }
}

/// Phase 3 (R16 tail): run C4a reconciliation, interruptibly.
///
/// A shutdown request during the scan abandons the pass — the next daemon
/// generation redoes it idempotently — and `None` tells the caller to take the
/// pre-listener drain instead of advertising `Ready`.
async fn reconcile_before_ready(
    store: &SqliteStoreHandle,
    device_id: &DeviceId,
    shutdown: &mut watch::Receiver<ShutdownRequest>,
) -> Option<Result<(), haider_protocol::error::HaiderError>> {
    let mut recovery = Box::pin(reconcile_dispatched_effects(store, device_id));
    let outcome = loop {
        if !matches!(*shutdown.borrow(), ShutdownRequest::None) {
            break None;
        }
        tokio::select! {
            result = &mut recovery => break Some(result),
            changed = shutdown.changed() => {
                if changed.is_err() || !matches!(*shutdown.borrow(), ShutdownRequest::None) {
                    break None;
                }
            }
        }
    };
    drop(recovery);
    outcome.map(|result| result.map(|_| ()))
}

/// Phase 4+5 state: the connection tasks, their writer tasks, and the
/// admission permits — everything the accept loop creates and the drain
/// barrier must account for.
struct ConnectionRuntime {
    connections: JoinSet<Result<ConnectionExit, DaemonError>>,
    /// Writer handles registered by connections. Teardown owns their abort and
    /// their JOIN; the connection itself only holds an abort handle (R17).
    writers: Vec<JoinHandle<()>>,
    writer_receiver: tokio::sync::mpsc::UnboundedReceiver<JoinHandle<()>>,
    /// Admission cap: one permit per served connection, owned by that
    /// connection's task so it is returned on normal completion, on error, and
    /// on abort alike. Nothing about listener close or drain ordering depends
    /// on it (report §2.5).
    admission: Arc<Semaphore>,
    connection_admissions: Arc<AtomicU64>,
}

#[derive(Debug, Clone)]
enum RuntimeStop {
    Shutdown(ShutdownRequest),
    Crash,
}

impl ConnectionRuntime {
    fn new(
        max_connections: usize,
        writer_receiver: tokio::sync::mpsc::UnboundedReceiver<JoinHandle<()>>,
        connection_admissions: Arc<AtomicU64>,
    ) -> Self {
        Self {
            connections: JoinSet::new(),
            writers: Vec::new(),
            writer_receiver,
            admission: Arc::new(Semaphore::new(max_connections)),
            connection_admissions,
        }
    }

    /// Phase 4: serve until a shutdown request (or a listener failure, which is
    /// reported alongside the graceful request it triggers).
    async fn accept_until_shutdown(
        &mut self,
        endpoint: &endpoint::BoundEndpoint,
        context: &ConnectionContext,
        drain_receiver: &watch::Receiver<Option<DrainNotice>>,
        shutdown: &mut watch::Receiver<ShutdownRequest>,
        crash: &mut watch::Receiver<bool>,
    ) -> (RuntimeStop, Option<DaemonError>) {
        let mut listener_error = None;
        let mut idle_wait_logged = None;
        #[cfg(unix)]
        let accept_operation = "accept Unix connection";
        #[cfg(windows)]
        let accept_operation = "accept Windows named-pipe connection";
        let stop = loop {
            if *crash.borrow() {
                break RuntimeStop::Crash;
            }
            match shutdown.borrow().clone() {
                request @ (ShutdownRequest::Graceful { .. } | ShutdownRequest::Forced { .. }) => {
                    break RuntimeStop::Shutdown(request);
                }
                ShutdownRequest::GracefulWhenIdle { reason } => {
                    let attached_clients = self.connections.len();
                    if attached_clients == 0 {
                        eprintln!(
                            "haiderd: ephemeral-lifecycle event=shutdown_decision reason=launcher_vanished attached_clients=0 decision=shutdown"
                        );
                        tracing::info!(
                            attached_clients,
                            reason = %reason,
                            decision = "shutdown",
                            "ephemeral daemon reached idle shutdown"
                        );
                        break RuntimeStop::Shutdown(ShutdownRequest::Graceful { reason });
                    }
                    if idle_wait_logged != Some(attached_clients) {
                        idle_wait_logged = Some(attached_clients);
                        eprintln!(
                            "haiderd: spawning client vanished; ephemeral daemon is waiting for live clients to disconnect; ephemeral-lifecycle event=shutdown_decision attached_clients={attached_clients} decision=stay_alive"
                        );
                        tracing::info!(
                            attached_clients,
                            reason = %reason,
                            decision = "stay_alive",
                            "spawning client vanished; ephemeral daemon is waiting for live clients to disconnect"
                        );
                    }
                }
                ShutdownRequest::None => {}
            }
            tokio::select! {
                changed = crash.changed() => {
                    if changed.is_err() || *crash.borrow() {
                        break RuntimeStop::Crash;
                    }
                }
                changed = shutdown.changed() => {
                    if changed.is_err() {
                        break RuntimeStop::Shutdown(ShutdownRequest::Forced {
                            reason: "shutdown controller dropped".into(),
                        });
                    }
                }
                accepted = endpoint.accept() => {
                    match accepted {
                        Ok((stream, _)) => {
                            match Arc::clone(&self.admission).try_acquire_owned() {
                                Ok(permit) => {
                                    self.connection_admissions.fetch_add(1, Ordering::Relaxed);
                                    let connection_context = context.clone();
                                    let connection_drain = drain_receiver.clone();
                                    self.connections.spawn(async move {
                                        // The owned permit lives inside the task, so
                                        // every exit path — return, error, or abort —
                                        // frees the slot exactly once.
                                        let _permit = permit;
                                        serve(stream, connection_context, connection_drain).await
                                    });
                                    let attached_clients = self.connections.len();
                                    eprintln!(
                                        "haiderd: ephemeral-lifecycle event=connection_admitted attached_clients={attached_clients}"
                                    );
                                    tracing::info!(
                                        attached_clients,
                                        "daemon connection admitted"
                                    );
                                }
                                // Over the cap: typed rejection, then close. No task
                                // and no queue is created for this peer.
                                Err(_) => reject_over_limit(&stream, context),
                            }
                        }
                        Err(error) => {
                            listener_error = Some(DaemonError::io(
                                accept_operation,
                                endpoint.path(),
                                error,
                            ));
                            break RuntimeStop::Shutdown(ShutdownRequest::Graceful {
                                reason: "listener failure".into(),
                            });
                        }
                    }
                }
                registered = self.writer_receiver.recv() => {
                    if let Some(handle) = registered {
                        // Opportunistic pruning keeps the registry proportional to
                        // live connections, not to connections ever served.
                        self.writers.retain(|writer: &JoinHandle<()>| !writer.is_finished());
                        self.writers.push(handle);
                    }
                }
                completed = self.connections.join_next(), if !self.connections.is_empty() => {
                    let attached_clients = self.connections.len();
                    journal_connection_exit(&completed, attached_clients);
                }
            }
        };
        (stop, listener_error)
    }

    /// Abruptly aborts and joins every connection/writer child without a
    /// `ServerDraining` broadcast.
    async fn crash(&mut self) {
        self.collect_writers();
        self.connections.abort_all();
        for writer in &self.writers {
            writer.abort();
        }
        while self.connections.join_next().await.is_some() {}
        self.collect_writers();
        for writer in &self.writers {
            writer.abort();
        }
        for writer in std::mem::take(&mut self.writers) {
            let _ = writer.await;
        }
    }

    /// Phase 5a: bounded completion, then take ownership of every child.
    ///
    /// Returns how many connections the daemon could not hand their last frame
    /// to. `forced` is raised — never lowered — by anything that breaches the
    /// barrier here.
    async fn drain(
        &mut self,
        forced: &mut bool,
        deadline: tokio::time::Instant,
        shutdown: &mut watch::Receiver<ShutdownRequest>,
    ) -> usize {
        let mut undelivered = 0_usize;
        if !*forced {
            // One scheduling turn so a connection that is already parked on the
            // drain broadcast can react before the daemon starts waiting on it;
            // it also lets a force that raced the broadcast be seen below.
            tokio::task::yield_now().await;
            *forced = matches!(*shutdown.borrow(), ShutdownRequest::Forced { .. });
        }
        if !*forced {
            while !self.connections.is_empty() {
                tokio::select! {
                    completed = self.connections.join_next() => {
                        let attached_clients = self.connections.len();
                        note_connection_exit(&mut undelivered, &completed, attached_clients);
                    }
                    changed = shutdown.changed() => {
                        if changed.is_err()
                            || matches!(*shutdown.borrow(), ShutdownRequest::Forced { .. })
                        {
                            *forced = true;
                            break;
                        }
                    }
                    () = tokio::time::sleep_until(deadline) => {
                        *forced = true;
                        break;
                    }
                }
            }
        }
        *forced |= barrier_breached(deadline, shutdown);
        // Collect writers registered since the accept loop's last turn, then
        // take ownership of their completion: abort first so every connection
        // parked on its writer can finish, and JOIN below so no writer future —
        // with its socket half and payload — can still exist when the endpoint
        // and the store go.
        self.collect_writers();
        if *forced {
            for writer in &self.writers {
                writer.abort();
            }
            self.connections.abort_all();
        }
        while let Some(completed) = self.connections.join_next().await {
            let attached_clients = self.connections.len();
            note_connection_exit(&mut undelivered, &Some(completed), attached_clients);
        }
        // RE-DRAIN. A connection accepted just before shutdown can register its
        // writer AFTER the collection above — its first poll may run on another
        // worker while this task is already draining. Every sender is a
        // connection task (the runtime's own context clone is a sender that
        // never sends), and all of them are joined by the line above, so this
        // second collection is provably the last one.
        self.collect_writers();
        if *forced {
            for writer in &self.writers {
                writer.abort();
            }
        }
        let writers = std::mem::take(&mut self.writers);
        let joined = bounded_finalization(
            async {
                for writer in writers {
                    let _ = writer.await;
                }
            },
            deadline,
            shutdown,
        )
        .await;
        // An unjoined writer is exactly what P1-1 forbids: report it, never
        // pretend the barrier completed cleanly.
        *forced |= joined.is_none() || barrier_breached(deadline, shutdown);
        undelivered
    }

    fn collect_writers(&mut self) {
        while let Ok(handle) = self.writer_receiver.try_recv() {
            self.writers.push(handle);
        }
    }
}

fn daemon_error_class(error: &DaemonError) -> &'static str {
    match error {
        DaemonError::AlreadyRunning { .. } => "already_running",
        DaemonError::InvalidConfig { .. } => "invalid_config",
        DaemonError::Store(_) => "store",
        DaemonError::Io { .. } => "io",
        DaemonError::RuntimeDirectoryNotEmpty { .. } => "runtime_directory_not_empty",
        DaemonError::EndpointAddressTooLong { .. } => "endpoint_address_too_long",
        DaemonError::Endpoint { .. } => "endpoint",
        DaemonError::Protocol { .. } => "protocol",
        DaemonError::Task { .. } => "task",
        DaemonError::Lockdown { .. } => "lockdown",
    }
}

fn daemon_error_raw_os_error(error: &DaemonError) -> Option<i32> {
    match error {
        DaemonError::Io { source, .. } => source.raw_os_error(),
        _ => None,
    }
}

/// Phase 5b: flush, remove the exact owned socket/pid/runtime, close the store
/// (lock release) LAST. Every step runs under the same barrier discipline;
/// the returned error is the first one worth reporting, in that order.
async fn finalize(
    store: SqliteStoreHandle,
    endpoint: &mut endpoint::BoundEndpoint,
    deadline: tokio::time::Instant,
    shutdown: &mut watch::Receiver<ShutdownRequest>,
    forced: &mut bool,
) -> Option<DaemonError> {
    let flush_error = barrier_step(
        store.flush(),
        Some("store_flush"),
        StepFailure::SuppressedWhenForced,
        deadline,
        shutdown,
        forced,
    )
    .await;
    // Socket removal happens even when the flush overran: an abandoned
    // rendezvous node is worse than a large WAL.
    let cleanup_error = barrier_step(
        std::future::ready(endpoint.cleanup()),
        None,
        StepFailure::AlwaysReported,
        deadline,
        shutdown,
        forced,
    )
    .await;
    let runtime_error = barrier_step(
        std::future::ready(endpoint.cleanup_runtime()),
        None,
        StepFailure::AlwaysReported,
        deadline,
        shutdown,
        forced,
    )
    .await;
    let close_error = barrier_step(
        store.close(),
        Some("store_close"),
        StepFailure::SuppressedWhenForced,
        deadline,
        shutdown,
        forced,
    )
    .await;
    flush_error
        .or(cleanup_error)
        .or(runtime_error)
        .or(close_error)
}

/// What a barrier step's failure means once the barrier has been breached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepFailure {
    /// Lossy by contract (R17): a store flush or close that fails BECAUSE THIS
    /// STEP ran into the barrier — its own overrun, an expired deadline, or a
    /// second signal — is an expected consequence of the forced path, not the
    /// daemon's report. A force raised for an unrelated reason does not
    /// suppress it.
    SuppressedWhenForced,
    /// Reported on every path: a rendezvous node the daemon could not remove
    /// outlives the process and confuses the next start, forced or not.
    AlwaysReported,
}

/// Runs one step of the barrier and arbitrates it.
///
/// This is the single place the barrier's idiom lives: bound the step by the
/// deadline and the second-signal escape, then re-check BOTH afterwards,
/// because a step completing is not evidence that the barrier held. `forced` is
/// only ever raised. A synchronous step passes `std::future::ready(..)` so it
/// gets the same arbitration.
async fn barrier_step<T, E>(
    work: impl Future<Output = Result<T, E>>,
    journal_step: Option<&'static str>,
    failure: StepFailure,
    deadline: tokio::time::Instant,
    shutdown: &mut watch::Receiver<ShutdownRequest>,
    forced: &mut bool,
) -> Option<DaemonError>
where
    DaemonError: From<E>,
{
    if let Some(step) = journal_step {
        eprintln!("haiderd: ephemeral-lifecycle event=cleanup_step step={step} outcome=started");
    }
    let outcome = bounded_finalization(work, deadline, shutdown).await;
    let overran = outcome.is_none();
    let error = outcome
        .and_then(|result| result.err())
        .map(DaemonError::from);
    // Suppression keys on WHY, not on the accumulated flag: only a barrier the
    // step itself ran into — its own overrun, an expired deadline, a second
    // signal — makes that step's failure an expected consequence. A `forced`
    // raised elsewhere (an undelivered drain notice, say) says nothing about
    // this step, and must not swallow an unrelated store error.
    let breached = overran || barrier_breached(deadline, shutdown);
    *forced |= breached;
    if let Some(step) = journal_step {
        if overran {
            eprintln!(
                "haiderd: ephemeral-lifecycle event=cleanup_step step={step} outcome=barrier_overrun"
            );
        } else if let Some(error) = error.as_ref() {
            eprintln!(
                "haiderd: ephemeral-lifecycle event=cleanup_step step={step} outcome=failed error_class={} raw_os_error={:?}",
                daemon_error_class(error),
                daemon_error_raw_os_error(error)
            );
        } else {
            eprintln!(
                "haiderd: ephemeral-lifecycle event=cleanup_step step={step} outcome=completed barrier_breached={breached}"
            );
        }
    }
    match failure {
        StepFailure::SuppressedWhenForced if breached => None,
        StepFailure::SuppressedWhenForced | StepFailure::AlwaysReported => error,
    }
}

type ConnectionCompletion =
    Option<Result<Result<ConnectionExit, DaemonError>, tokio::task::JoinError>>;

fn journal_connection_exit(completed: &ConnectionCompletion, attached_clients: usize) {
    match completed {
        Some(Ok(Ok(ConnectionExit::ClosedBeforeDrain { reason }))) => {
            eprintln!(
                "haiderd: ephemeral-lifecycle event=connection_retired reason={} attached_clients={attached_clients}",
                reason.as_str()
            );
            tracing::info!(
                reason = reason.as_str(),
                attached_clients,
                "daemon connection retired"
            );
        }
        Some(Ok(Ok(
            exit @ (ConnectionExit::NoticeDelivered | ConnectionExit::NoticeUndelivered),
        ))) => {
            eprintln!(
                "haiderd: ephemeral-lifecycle event=connection_retired reason=daemon_shutdown attached_clients={attached_clients} drain_outcome={exit:?}"
            );
            tracing::info!(
                ?exit,
                reason = "daemon_shutdown",
                attached_clients,
                "daemon connection retired"
            );
        }
        Some(Ok(Err(error))) => {
            eprintln!(
                "haiderd: ephemeral-lifecycle event=connection_retired reason=error attached_clients={attached_clients} error_class={} raw_os_error={:?}",
                daemon_error_class(error),
                daemon_error_raw_os_error(error)
            );
            tracing::warn!(%error, attached_clients, "daemon connection failed");
        }
        Some(Err(error)) => {
            eprintln!(
                "haiderd: ephemeral-lifecycle event=connection_retired reason=task_error attached_clients={attached_clients} task_cancelled={} task_panicked={}",
                error.is_cancelled(),
                error.is_panic()
            );
            tracing::warn!(%error, attached_clients, "daemon connection task failed");
        }
        None => {}
    }
}

/// Counts connections whose last frame never made it out (R17 honesty). A
/// connection that simply failed its socket is not counted: that is the peer's
/// end going away, not the daemon skipping its notice. Every completion is
/// journaled here as well because shutdown-drain retirements bypass the accept
/// loop's normal completion branch.
fn note_connection_exit(
    undelivered: &mut usize,
    completed: &ConnectionCompletion,
    attached_clients: usize,
) {
    if let Some(Ok(Ok(ConnectionExit::NoticeUndelivered))) = completed {
        *undelivered = undelivered.saturating_add(1);
    }
    journal_connection_exit(completed, attached_clients);
}

/// Runs one finalization step under the drain deadline and the second-signal
/// escape. `None` means the deadline expired or a force arrived — the caller
/// takes the forced path and never reports `Graceful`.
///
/// The step is always STARTED, even when the barrier is already over: the store
/// work is what releases the profile lock, and a blocking SQLite call cannot be
/// cancelled anyway. The deadline decides only whether this task keeps waiting;
/// abandoned work finishes on the blocking pool and releases the lock as soon
/// as it physically can.
async fn bounded_finalization<T>(
    work: impl Future<Output = T>,
    deadline: tokio::time::Instant,
    shutdown: &mut watch::Receiver<ShutdownRequest>,
) -> Option<T> {
    tokio::pin!(work);
    // Exactly one poll: enough to START the step (which spawns its blocking
    // store call), never enough to wait for it. A zero-duration timeout will
    // not do — its timer still parks until the next tick, which is long enough
    // for the step to finish and hide an already-expired deadline.
    let started = std::future::poll_fn(|context| Poll::Ready(work.as_mut().poll(context))).await;
    if let Poll::Ready(output) = started {
        // Completing is not evidence the barrier held; the caller arbitrates
        // the deadline and the force AFTER every step.
        return Some(output);
    }
    if barrier_breached(deadline, shutdown) {
        return None;
    }
    loop {
        tokio::select! {
            // Biased: the barrier's own signals are inspected before the work,
            // so a step that becomes ready at the same instant as the deadline
            // or a force can never mask them.
            biased;
            () = tokio::time::sleep_until(deadline) => return None,
            changed = shutdown.changed() => {
                match changed {
                    // A second signal during finalization forces, like anywhere
                    // else in the barrier.
                    Ok(()) if matches!(*shutdown.borrow(), ShutdownRequest::Forced { .. }) => {
                        return None;
                    }
                    Ok(()) => {}
                    // A dropped controller is not a second signal: nobody can
                    // force any more, so keep waiting on the deadline alone.
                    Err(_) => return tokio::time::timeout_at(deadline, &mut work).await.ok(),
                }
            }
            output = &mut work => return Some(output),
        }
    }
}

/// Has the drain barrier been breached — deadline gone, or a force arrived?
///
/// Read from the watch value rather than from `changed()`, so a second signal
/// delivered while a SYNCHRONOUS step ran is still observed (R17 honesty).
pub(crate) fn barrier_breached(
    deadline: tokio::time::Instant,
    shutdown: &watch::Receiver<ShutdownRequest>,
) -> bool {
    tokio::time::Instant::now() >= deadline
        || matches!(*shutdown.borrow(), ShutdownRequest::Forced { .. })
}

/// Drain tail for shutdown observed after store open but before the listener
/// bound: no socket or connections exist yet, so the barrier reduces to
/// publish Draining -> flush -> remove runtime -> close (lock release last).
/// The same single deadline and second-signal escape bound this tail as bound
/// the full barrier — a slow flush here must not hold the profile lock either.
async fn shutdown_before_listener(
    config: &DaemonConfig,
    states: &StatePublisher,
    store: SqliteStoreHandle,
    mut runtime_directory: endpoint::RuntimeDirectory,
    request: ShutdownRequest,
    shutdown: &mut watch::Receiver<ShutdownRequest>,
) -> Result<ShutdownOutcome, DaemonError> {
    let (reason, mut forced) = match request {
        ShutdownRequest::Graceful { reason } | ShutdownRequest::GracefulWhenIdle { reason } => {
            (reason.to_string(), false)
        }
        ShutdownRequest::Forced { reason } => (reason.to_string(), true),
        // `None` means the ShutdownHandle was dropped without a request
        // (watch channel closed mid-recovery); treat as forced.
        ShutdownRequest::None => ("startup shutdown controller dropped".into(), true),
    };
    let barrier_deadline = tokio::time::Instant::now() + config.drain_timeout;
    states.publish(DaemonState::Draining {
        reason,
        deadline_unix_ms: unix_time_ms().saturating_add(duration_ms(config.drain_timeout)),
    });
    // The same barrier steps the full drain runs, through the same helper:
    // there is no socket and no connection here, so the tail is
    // flush → runtime cleanup → close.
    let flush_error = barrier_step(
        store.flush(),
        Some("store_flush"),
        StepFailure::SuppressedWhenForced,
        barrier_deadline,
        shutdown,
        &mut forced,
    )
    .await;
    let runtime_error = barrier_step(
        std::future::ready(runtime_directory.cleanup()),
        None,
        StepFailure::AlwaysReported,
        barrier_deadline,
        shutdown,
        &mut forced,
    )
    .await;
    let close_error = barrier_step(
        store.close(),
        Some("store_close"),
        StepFailure::SuppressedWhenForced,
        barrier_deadline,
        shutdown,
        &mut forced,
    )
    .await;
    if let Some(error) = flush_error.or(runtime_error).or(close_error) {
        return Err(error);
    }
    states.publish(DaemonState::Stopped);
    Ok(if forced {
        ShutdownOutcome::Forced
    } else {
        ShutdownOutcome::Graceful
    })
}

/// Drain tail for shutdown observed before the profile lock or store exist:
/// nothing was acquired, so only the phase transitions are published. This
/// path must not consume a worker/daemon generation.
fn shutdown_without_store(
    config: &DaemonConfig,
    states: &StatePublisher,
    request: ShutdownRequest,
    shutdown: &watch::Receiver<ShutdownRequest>,
) -> Result<ShutdownOutcome, DaemonError> {
    let (reason, mut forced) = match request {
        ShutdownRequest::Graceful { reason } | ShutdownRequest::GracefulWhenIdle { reason } => {
            (reason.to_string(), false)
        }
        ShutdownRequest::Forced { reason } => (reason.to_string(), true),
        ShutdownRequest::None => ("startup shutdown controller dropped".into(), true),
    };
    let barrier_deadline = tokio::time::Instant::now() + config.drain_timeout;
    states.publish(DaemonState::Draining {
        reason,
        deadline_unix_ms: unix_time_ms().saturating_add(duration_ms(config.drain_timeout)),
    });
    // Same arbitration as the other two tails: the caller handed us a CLONE of
    // the request, so a second signal that landed after that clone is only
    // visible in the CURRENT watch value. Without this recheck the caller could
    // be told `Forced` while the daemon's own join reported `Graceful` — all
    // three shutdown paths must agree on one answer.
    forced |= barrier_breached(barrier_deadline, shutdown);
    states.publish(DaemonState::Stopped);
    Ok(if forced {
        ShutdownOutcome::Forced
    } else {
        ShutdownOutcome::Graceful
    })
}

/// Best-effort description of whoever holds the profile lock. Read-only and
/// advisory (R1): nothing here is probed for liveness or trusted for
/// decisions — the lock itself already decided. Diagnostics live beside the
/// empty lock file so Windows' mandatory lock cannot block this read.
fn incumbent_diagnostics(config: &DaemonConfig, endpoint_path: &Path) -> IncumbentDiagnostics {
    let owner_path = config.store_dir.join("lock.owner");
    let lock_contents = fs::read_to_string(owner_path)
        .ok()
        .map(|contents| contents.chars().take(4_096).collect());
    IncumbentDiagnostics {
        profile_id: config.profile_id.clone(),
        endpoint_path: endpoint_path.to_path_buf(),
        lock_contents,
    }
}

fn random_instance_id() -> Result<String, DaemonError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| DaemonError::Task {
        message: format!("cannot generate daemon instance identity: {error}"),
    })?;
    let mut id = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut id, "{byte:02x}").map_err(|error| DaemonError::Task {
            message: format!("cannot format daemon instance identity: {error}"),
        })?;
    }
    Ok(id)
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}
