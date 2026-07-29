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
use crate::endpoint;
use crate::lifecycle::{ShutdownObserver, ShutdownRequest, StatePublisher};
use crate::turn_recovery::{RecoveredWork, recover_interrupted_turns};
use crate::worker::WorkerManager;
use crate::{
    DaemonConfig, DaemonDependencies, DaemonError, DaemonState, IncumbentDiagnostics, Readiness,
    SessionHub, ShutdownHandle, ShutdownOutcome,
};
use haider_core::{SqliteStoreHandle, reconcile_dispatched_effects};
use haider_protocol::error::ErrorCode;
use haider_protocol::ids::DeviceId;
use std::fs;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{Semaphore, watch};
use tokio::task::{JoinHandle, JoinSet};

/// Handle to one spawned daemon: observe its phases, request shutdown, and
/// join for the typed outcome.
pub struct DaemonTask {
    readiness: Readiness,
    shutdown: ShutdownHandle,
    crash: watch::Sender<bool>,
    task: JoinHandle<Result<ShutdownOutcome, DaemonError>>,
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
    let (states, readiness) = StatePublisher::channel();
    let (shutdown, shutdown_receiver, shutdown_observer) = ShutdownHandle::channel();
    let (crash, crash_receiver) = watch::channel(false);
    let task = tokio::spawn(run_owner(
        config,
        dependencies,
        states,
        shutdown_receiver,
        shutdown_observer,
        crash_receiver,
    ));
    DaemonTask {
        readiness,
        shutdown,
        crash,
        task,
    }
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
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|error| DaemonError::Task {
        message: format!("cannot install SIGTERM handler: {error}"),
    })?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .map_err(|error| DaemonError::Task {
        message: format!("cannot install SIGINT handler: {error}"),
    })?;
    let task = spawn_with_dependencies(config, dependencies);
    let shutdown = task.shutdown_handle();
    let mut joined: Pin<Box<dyn Future<Output = Result<ShutdownOutcome, DaemonError>> + Send>> =
        Box::pin(task.join());
    loop {
        tokio::select! {
            result = &mut joined => return result,
            signal = terminate.recv() => {
                if signal.is_some() {
                    shutdown.request("SIGTERM");
                }
            }
            signal = interrupt.recv() => {
                if signal.is_some() {
                    shutdown.request("SIGINT");
                }
            }
        }
    }
}

async fn run_owner(
    config: DaemonConfig,
    dependencies: DaemonDependencies,
    states: StatePublisher,
    shutdown: watch::Receiver<ShutdownRequest>,
    shutdown_observer: ShutdownObserver,
    crash: watch::Receiver<bool>,
) -> Result<ShutdownOutcome, DaemonError> {
    let result = run_inner(
        &config,
        dependencies,
        &states,
        shutdown,
        &shutdown_observer,
        crash,
    )
    .await;
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
    mut shutdown: watch::Receiver<ShutdownRequest>,
    shutdown_observer: &ShutdownObserver,
    mut crash: watch::Receiver<bool>,
) -> Result<ShutdownOutcome, DaemonError> {
    config
        .validate()
        .map_err(|message| DaemonError::InvalidConfig { message })?;
    if !matches!(*shutdown.borrow(), ShutdownRequest::None) {
        let request = shutdown.borrow().clone();
        return shutdown_without_store(config, states, request, &shutdown);
    }
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
    if !matches!(*shutdown.borrow(), ShutdownRequest::None) {
        let request = shutdown.borrow().clone();
        drop(lease);
        return shutdown_without_store(config, states, request, &shutdown);
    }

    // R16 ready gate: open store under the lock -> durable generation bump ->
    // reconcile every dispatched-without-terminal effect. Only after all of
    // this may a listener bind or Ready be advertised.
    states.publish(DaemonState::Recovering);
    let store = SqliteStoreHandle::open_locked(lease).await?;
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
            return shutdown_before_listener(config, states, store, request, &mut shutdown).await;
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
        return shutdown_before_listener(config, states, store, request, &mut shutdown).await;
    }
    let turn_recovery_started = Instant::now();
    let recovered_work = match recover_interrupted_turns(&store, &device_id).await {
        Ok(work) => work,
        Err(error) => {
            let _ = store.close().await;
            return Err(error.into());
        }
    };
    tracing::trace!(
        target: "haider.recovery",
        phase = "turns",
        recovered_work = recovered_work.len(),
        operation_micros = turn_recovery_started.elapsed().as_micros(),
        "pre-ready recovery phase completed"
    );

    // W3c2 R10 startup phase: load the descriptor store and reconcile
    // pending/committed LOGIN receipts against vault + descriptor truth
    // before anything can observe Ready (run_inner's receipt-reconciliation
    // phase — W3c1 receipts never persist `pending`; login's can).
    let accounts_started = tokio::time::Instant::now();
    let accounts_runtime = match crate::accounts::AccountsRuntime::initialize(
        &store,
        &dependencies.accounts,
        &config.store_dir,
        &config.profile_id,
        &instance_id,
        &config.default_model,
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

    let hub = SessionHub::new(store.clone(), config.session_hub).map_err(DaemonError::from)?;
    // D3-5 whitelist unification + the production factory swap: the ONE
    // provider authority is the dependency configuration. `Accounts` (the
    // default) resolves per logical turn from the daemon-owned account
    // snapshot + vault, so a committed login is picked up by the NEXT
    // logical turn; `"fake"` is creatable only under an injected test
    // configuration, never on the production wire path.
    let creatable_providers = dependencies.provider_factory.creatable_providers();
    let provider_factory: std::sync::Arc<dyn crate::worker::ProviderFactory> = match &dependencies
        .provider_factory
    {
        crate::worker::ProviderFactoryConfig::Accounts => match accounts_runtime.broker.clone() {
            Some(broker) => {
                std::sync::Arc::new(crate::accounts::AccountsProviderFactory::with_broker(
                    std::sync::Arc::clone(&accounts_runtime.facade.snapshot),
                    accounts_runtime.vault.clone(),
                    std::sync::Arc::new(crate::accounts::ProductionAccountBuilder),
                    broker,
                ))
            }
            None => std::sync::Arc::new(crate::accounts::AccountsProviderFactory::new(
                std::sync::Arc::clone(&accounts_runtime.facade.snapshot),
                accounts_runtime.vault.clone(),
                std::sync::Arc::new(crate::accounts::ProductionAccountBuilder),
            )),
        },
        crate::worker::ProviderFactoryConfig::AccountsWith(builder) => {
            match accounts_runtime.broker.clone() {
                Some(broker) => {
                    std::sync::Arc::new(crate::accounts::AccountsProviderFactory::with_broker(
                        std::sync::Arc::clone(&accounts_runtime.facade.snapshot),
                        accounts_runtime.vault.clone(),
                        std::sync::Arc::clone(builder),
                        broker,
                    ))
                }
                None => std::sync::Arc::new(crate::accounts::AccountsProviderFactory::new(
                    std::sync::Arc::clone(&accounts_runtime.facade.snapshot),
                    accounts_runtime.vault.clone(),
                    std::sync::Arc::clone(builder),
                )),
            }
        }
        crate::worker::ProviderFactoryConfig::Injected { factory, .. } => {
            std::sync::Arc::clone(factory)
        }
    };
    hub.install_creatable_providers(creatable_providers)
        .map_err(DaemonError::from)?;
    let worker_dependencies = crate::worker::WorkerDependencies {
        provider_factory,
        tool_factory: std::sync::Arc::clone(&dependencies.tool_factory),
    };
    let worker_manager = WorkerManager::start(hub.clone(), worker_dependencies);
    let worker_handle = worker_manager.handle();
    hub.install_worker_manager(worker_handle.clone())
        .map_err(DaemonError::from)?;
    let crate::accounts::AccountsRuntime {
        facade: accounts_facade,
        actor: account_actor,
        vault: _,
        broker: credential_broker,
    } = accounts_runtime;
    let oauth_coordinator = accounts_facade.oauth.clone();
    hub.install_accounts(accounts_facade)
        .map_err(DaemonError::from)?;
    for work in recovered_work {
        let result = match work {
            RecoveredWork::Queued(accepted) => worker_handle.recover_queued(accepted).await,
            RecoveredWork::Checkpoint(recovered) => {
                worker_handle
                    .recover_checkpoint(
                        recovered.accepted,
                        recovered.checkpoint,
                        recovered.committed_answer,
                    )
                    .await
            }
        };
        if let Err(error) = result {
            let _ = worker_manager.shutdown().await;
            let _ = hub.shutdown().await;
            let _ = store.close().await;
            return Err(error.into());
        }
    }
    let mut endpoint = match endpoint::bind(config).await {
        Ok(endpoint) => endpoint,
        Err(error) => {
            let _ = worker_manager.shutdown().await;
            let _ = store.flush().await;
            let _ = store.close().await;
            return Err(error);
        }
    };
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
        owner_uid: endpoint.owner_uid,
        hub: hub.clone(),
        endpoint_path: endpoint.path().to_path_buf(),
    };
    // Ready is published under the shutdown transition mutex, so a first
    // signal that races this point either wins (no Ready, drain from
    // Recovering) or loses (Ready, then a normal drain).
    shutdown_observer.publish_ready_if_idle(states);

    let mut runtime = ConnectionRuntime::new(config.max_connections, writer_receiver);
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
        runtime.crash().await;
        worker_manager.crash().await;
        if let Some(broker) = &credential_broker {
            broker.abort_and_join().await;
        }
        if let Some(oauth) = &oauth_coordinator {
            oauth.abort_and_join().await;
        }
        if let Some(actor) = account_actor {
            actor.crash();
        }
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
    endpoint.close_listener();
    let (reason, mut forced) = match request {
        ShutdownRequest::Graceful { reason } => (reason, false),
        ShutdownRequest::Forced { reason } => (reason, true),
        // Unreachable: the loop above only breaks with a real request.
        ShutdownRequest::None => ("internal shutdown".into(), true),
    };
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
    match worker_shutdown {
        Some(Ok(())) => {}
        Some(Err(error)) => return Err(error.into()),
        None => forced = true,
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
    let hub_shutdown = bounded_finalization(hub.shutdown(), barrier_deadline, &mut shutdown).await;
    match hub_shutdown {
        Some(Ok(crate::SessionHubShutdownOutcome::Graceful)) => {}
        Some(Ok(crate::SessionHubShutdownOutcome::Forced)) => forced = true,
        Some(Err(error)) => return Err(DaemonError::from(error)),
        None => forced = true,
    }
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
    // whatever the barrier then reported.
    if let Some(error) = listener_error.or(finalize_error) {
        return Err(error);
    }
    states.publish(DaemonState::Stopped);
    Ok(if forced {
        ShutdownOutcome::Forced
    } else {
        ShutdownOutcome::Graceful
    })
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
    ) -> Self {
        Self {
            connections: JoinSet::new(),
            writers: Vec::new(),
            writer_receiver,
            admission: Arc::new(Semaphore::new(max_connections)),
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
        let stop = loop {
            if *crash.borrow() {
                break RuntimeStop::Crash;
            }
            match shutdown.borrow().clone() {
                request @ (ShutdownRequest::Graceful { .. } | ShutdownRequest::Forced { .. }) => {
                    break RuntimeStop::Shutdown(request);
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
                                    let connection_context = context.clone();
                                    let connection_drain = drain_receiver.clone();
                                    self.connections.spawn(async move {
                                        // The owned permit lives inside the task, so
                                        // every exit path — return, error, or abort —
                                        // frees the slot exactly once.
                                        let _permit = permit;
                                        serve(stream, connection_context, connection_drain).await
                                    });
                                }
                                // Over the cap: typed rejection, then close. No task
                                // and no queue is created for this peer.
                                Err(_) => reject_over_limit(&stream, context),
                            }
                        }
                        Err(error) => {
                            listener_error = Some(DaemonError::io(
                                "accept Unix connection",
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
                    match completed {
                        Some(Ok(Ok(exit))) => {
                            tracing::debug!(?exit, "daemon connection closed");
                        }
                        Some(Ok(Err(error))) => {
                            tracing::warn!(%error, "daemon connection failed");
                        }
                        Some(Err(error)) => {
                            tracing::warn!(%error, "daemon connection task failed");
                        }
                        None => {}
                    }
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
                        note_connection_exit(&mut undelivered, completed);
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
            note_connection_exit(&mut undelivered, Some(completed));
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

/// Phase 5b: flush, remove the exact owned socket, close the store (lock
/// release) LAST. Every step runs under the same barrier discipline; the
/// returned error is the first one worth reporting, in that order.
async fn finalize(
    store: SqliteStoreHandle,
    endpoint: &mut endpoint::BoundEndpoint,
    deadline: tokio::time::Instant,
    shutdown: &mut watch::Receiver<ShutdownRequest>,
    forced: &mut bool,
) -> Option<DaemonError> {
    let flush_error = barrier_step(
        store.flush(),
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
        StepFailure::AlwaysReported,
        deadline,
        shutdown,
        forced,
    )
    .await;
    let close_error = barrier_step(
        store.close(),
        StepFailure::SuppressedWhenForced,
        deadline,
        shutdown,
        forced,
    )
    .await;
    flush_error.or(cleanup_error).or(close_error)
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
    failure: StepFailure,
    deadline: tokio::time::Instant,
    shutdown: &mut watch::Receiver<ShutdownRequest>,
    forced: &mut bool,
) -> Option<DaemonError>
where
    DaemonError: From<E>,
{
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
    match failure {
        StepFailure::SuppressedWhenForced if breached => None,
        StepFailure::SuppressedWhenForced | StepFailure::AlwaysReported => error,
    }
}

/// Counts connections whose last frame never made it out (R17 honesty). A
/// connection that simply failed its socket is not counted: that is the peer's
/// end going away, not the daemon skipping its notice.
fn note_connection_exit(
    undelivered: &mut usize,
    completed: Option<Result<Result<ConnectionExit, DaemonError>, tokio::task::JoinError>>,
) {
    if let Some(Ok(Ok(ConnectionExit::NoticeUndelivered))) = completed {
        *undelivered = undelivered.saturating_add(1);
    }
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
/// publish Draining -> flush -> close (lock release last). The same single
/// deadline and second-signal escape bound this tail as bound the full
/// barrier — a slow flush here must not hold the profile lock either.
async fn shutdown_before_listener(
    config: &DaemonConfig,
    states: &StatePublisher,
    store: SqliteStoreHandle,
    request: ShutdownRequest,
    shutdown: &mut watch::Receiver<ShutdownRequest>,
) -> Result<ShutdownOutcome, DaemonError> {
    let (reason, mut forced) = match request {
        ShutdownRequest::Graceful { reason } => (reason, false),
        ShutdownRequest::Forced { reason } => (reason, true),
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
    // there is no socket and no connection here, so the tail is flush → close.
    let flush_error = barrier_step(
        store.flush(),
        StepFailure::SuppressedWhenForced,
        barrier_deadline,
        shutdown,
        &mut forced,
    )
    .await;
    let close_error = barrier_step(
        store.close(),
        StepFailure::SuppressedWhenForced,
        barrier_deadline,
        shutdown,
        &mut forced,
    )
    .await;
    if let Some(error) = flush_error.or(close_error) {
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
        ShutdownRequest::Graceful { reason } => (reason, false),
        ShutdownRequest::Forced { reason } => (reason, true),
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
/// decisions — the lock itself already decided.
fn incumbent_diagnostics(config: &DaemonConfig, endpoint_path: &Path) -> IncumbentDiagnostics {
    let lock_path = config.store_dir.join("lock");
    let lock_contents = fs::read_to_string(lock_path)
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
