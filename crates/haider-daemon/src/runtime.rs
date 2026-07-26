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
//!    the store is what releases the profile lock.
//!
//! Shutdown may arrive at any point; the early-exit helpers
//! ([`shutdown_without_store`], [`shutdown_before_listener`]) run the same
//! tail ordering with whatever resources exist so far.

use crate::connection::{ConnectionContext, DrainNotice, reject_over_limit, serve};
use crate::endpoint;
use crate::lifecycle::{ShutdownObserver, ShutdownRequest, StatePublisher};
use crate::{
    DaemonConfig, DaemonError, DaemonState, IncumbentDiagnostics, Readiness, ShutdownHandle,
    ShutdownOutcome,
};
use haider_core::{SqliteStoreHandle, reconcile_dispatched_effects};
use haider_protocol::error::ErrorCode;
use haider_protocol::ids::DeviceId;
use std::fs;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{Semaphore, watch};
use tokio::task::{JoinHandle, JoinSet};

/// Handle to one spawned daemon: observe its phases, request shutdown, and
/// join for the typed outcome.
pub struct DaemonTask {
    readiness: Readiness,
    shutdown: ShutdownHandle,
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
}

/// Starts one owned daemon task and returns observable lifecycle controls.
pub fn spawn(config: DaemonConfig) -> DaemonTask {
    let (states, readiness) = StatePublisher::channel();
    let (shutdown, shutdown_receiver, shutdown_observer) = ShutdownHandle::channel();
    let task = tokio::spawn(run_owner(
        config,
        states,
        shutdown_receiver,
        shutdown_observer,
    ));
    DaemonTask {
        readiness,
        shutdown,
        task,
    }
}

/// Runs until Unix termination signals stop the daemon.
///
/// The first SIGINT/SIGTERM starts the drain barrier; the second selects the
/// forced path. Crash/forced recovery belongs to the next daemon generation.
pub async fn run_with_signals(config: DaemonConfig) -> Result<ShutdownOutcome, DaemonError> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|error| DaemonError::Task {
        message: format!("cannot install SIGTERM handler: {error}"),
    })?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .map_err(|error| DaemonError::Task {
        message: format!("cannot install SIGINT handler: {error}"),
    })?;
    let task = spawn(config);
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
    states: StatePublisher,
    shutdown: watch::Receiver<ShutdownRequest>,
    shutdown_observer: ShutdownObserver,
) -> Result<ShutdownOutcome, DaemonError> {
    let result = run_inner(&config, &states, shutdown, &shutdown_observer).await;
    if let Err(error) = &result {
        states.publish(DaemonState::Failed {
            message: error.to_string(),
        });
    }
    result
}

async fn run_inner(
    config: &DaemonConfig,
    states: &StatePublisher,
    mut shutdown: watch::Receiver<ShutdownRequest>,
    shutdown_observer: &ShutdownObserver,
) -> Result<ShutdownOutcome, DaemonError> {
    config
        .validate()
        .map_err(|message| DaemonError::InvalidConfig { message })?;
    if !matches!(*shutdown.borrow(), ShutdownRequest::None) {
        let request = shutdown.borrow().clone();
        return shutdown_without_store(config, states, request);
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
        return shutdown_without_store(config, states, request);
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
    // Recovery is shutdown-interruptible: a request during the scan abandons
    // the pass (the next generation redoes it idempotently) and drains.
    let mut recovery = Box::pin(reconcile_dispatched_effects(&store, &device_id));
    let recovery_result = loop {
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
    match recovery_result {
        Some(Ok(_)) => {}
        Some(Err(error)) => {
            let _ = store.close().await;
            return Err(error.into());
        }
        None => {
            let request = shutdown.borrow().clone();
            return shutdown_before_listener(config, states, store, request).await;
        }
    }
    if !matches!(*shutdown.borrow(), ShutdownRequest::None) {
        let request = shutdown.borrow().clone();
        return shutdown_before_listener(config, states, store, request).await;
    }

    let mut endpoint = match endpoint::bind(config).await {
        Ok(endpoint) => endpoint,
        Err(error) => {
            let _ = store.flush().await;
            let _ = store.close().await;
            return Err(error);
        }
    };
    let (drain_sender, drain_receiver) = watch::channel(Option::<DrainNotice>::None);
    let context = ConnectionContext {
        profile_id: config.profile_id.clone(),
        instance_id: instance_id.clone(),
        daemon_generation,
        frame_limit: config.frame_limit,
        outbound_queue_capacity: config.outbound_queue_capacity,
        outbound_queued_bytes: config.outbound_queued_bytes,
        max_connections: config.max_connections,
        owner_uid: endpoint.owner_uid,
        endpoint_path: endpoint.path().to_path_buf(),
    };
    // Ready is published under the shutdown transition mutex, so a first
    // signal that races this point either wins (no Ready, drain from
    // Recovering) or loses (Ready, then a normal drain).
    shutdown_observer.publish_ready_if_idle(states);

    let mut connections = JoinSet::new();
    // Admission cap: one permit per served connection, owned by that
    // connection's task so it is returned on normal completion, on error, and
    // on abort alike. Nothing about listener close or drain ordering depends on
    // it (report §2.5).
    let admission = Arc::new(Semaphore::new(config.max_connections));
    let mut listener_error = None;
    let request = loop {
        match shutdown.borrow().clone() {
            request @ (ShutdownRequest::Graceful { .. } | ShutdownRequest::Forced { .. }) => {
                break request;
            }
            ShutdownRequest::None => {}
        }
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() {
                    break ShutdownRequest::Forced {
                        reason: "shutdown controller dropped".into(),
                    };
                }
            }
            accepted = endpoint.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        match Arc::clone(&admission).try_acquire_owned() {
                            Ok(permit) => {
                                let connection_context = context.clone();
                                let connection_drain = drain_receiver.clone();
                                connections.spawn(async move {
                                    // The owned permit lives inside the task, so
                                    // every exit path — return, error, or abort —
                                    // frees the slot exactly once.
                                    let _permit = permit;
                                    serve(stream, connection_context, connection_drain).await
                                });
                            }
                            // Over the cap: typed rejection, then close. No task
                            // and no queue is created for this peer.
                            Err(_) => reject_over_limit(&stream, &context),
                        }
                    }
                    Err(error) => {
                        listener_error = Some(DaemonError::io(
                            "accept Unix connection",
                            endpoint.path(),
                            error,
                        ));
                        break ShutdownRequest::Graceful {
                            reason: "listener failure".into(),
                        };
                    }
                }
            }
            completed = connections.join_next(), if !connections.is_empty() => {
                let _ = completed;
            }
        }
    };

    // R17 drain barrier, in order: stop accepting, publish Draining,
    // broadcast ServerDraining to every connection, bounded completion,
    // flush, remove the exact owned socket, close the store (lock release)
    // LAST.
    endpoint.close_listener();
    let (reason, mut forced) = match request {
        ShutdownRequest::Graceful { reason } => (reason, false),
        ShutdownRequest::Forced { reason } => (reason, true),
        // Unreachable: the loop above only breaks with a real request.
        ShutdownRequest::None => ("internal shutdown".into(), true),
    };
    let deadline_unix_ms = unix_time_ms().saturating_add(duration_ms(config.drain_timeout));
    states.publish(DaemonState::Draining {
        reason: reason.clone(),
        deadline_unix_ms,
    });
    drain_sender.send_replace(Some(DrainNotice {
        reason,
        instance_id,
        daemon_generation,
        deadline_unix_ms,
    }));

    if !forced {
        tokio::task::yield_now().await;
        forced = matches!(*shutdown.borrow(), ShutdownRequest::Forced { .. });
    }
    if !forced {
        let deadline = tokio::time::Instant::now() + config.drain_timeout;
        while !connections.is_empty() {
            tokio::select! {
                completed = connections.join_next() => {
                    let _ = completed;
                }
                changed = shutdown.changed() => {
                    if changed.is_err()
                        || matches!(*shutdown.borrow(), ShutdownRequest::Forced { .. })
                    {
                        forced = true;
                        break;
                    }
                }
                () = tokio::time::sleep_until(deadline) => {
                    forced = true;
                    break;
                }
            }
        }
    }
    if !forced {
        forced = matches!(*shutdown.borrow(), ShutdownRequest::Forced { .. });
    }
    if forced {
        connections.abort_all();
    }
    while connections.join_next().await.is_some() {}

    // Flush is attempted on both paths; only the graceful path treats its
    // failure as a daemon error (the forced path is already lossy by
    // contract). The store close below releases the profile lock — it must
    // stay the last resource released.
    let flush_result = store.flush().await.err().map(DaemonError::from);
    let flush_error = if forced { None } else { flush_result };
    let cleanup_error = endpoint.cleanup().err();
    let close_error = store.close().await.err().map(DaemonError::from);

    if let Some(error) = listener_error
        .or(flush_error)
        .or(cleanup_error)
        .or(close_error)
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

/// Drain tail for shutdown observed after store open but before the listener
/// bound: no socket or connections exist yet, so the barrier reduces to
/// publish Draining -> flush -> close (lock release last).
async fn shutdown_before_listener(
    config: &DaemonConfig,
    states: &StatePublisher,
    store: SqliteStoreHandle,
    request: ShutdownRequest,
) -> Result<ShutdownOutcome, DaemonError> {
    let (reason, forced) = match request {
        ShutdownRequest::Graceful { reason } => (reason, false),
        ShutdownRequest::Forced { reason } => (reason, true),
        // `None` means the ShutdownHandle was dropped without a request
        // (watch channel closed mid-recovery); treat as forced.
        ShutdownRequest::None => ("startup shutdown controller dropped".into(), true),
    };
    states.publish(DaemonState::Draining {
        reason,
        deadline_unix_ms: unix_time_ms().saturating_add(duration_ms(config.drain_timeout)),
    });
    let flush_error = store.flush().await.err().map(DaemonError::from);
    let close_error = store.close().await.err().map(DaemonError::from);
    if !forced && let Some(error) = flush_error.or(close_error) {
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
) -> Result<ShutdownOutcome, DaemonError> {
    let (reason, forced) = match request {
        ShutdownRequest::Graceful { reason } => (reason, false),
        ShutdownRequest::Forced { reason } => (reason, true),
        ShutdownRequest::None => ("startup shutdown controller dropped".into(), true),
    };
    states.publish(DaemonState::Draining {
        reason,
        deadline_unix_ms: unix_time_ms().saturating_add(duration_ms(config.drain_timeout)),
    });
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
