//! Top-level lifecycle ordering and task ownership.

use crate::connection::{ConnectionContext, DrainNotice, serve};
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
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};

pub struct DaemonTask {
    readiness: Readiness,
    shutdown: ShutdownHandle,
    task: JoinHandle<Result<ShutdownOutcome, DaemonError>>,
}

impl DaemonTask {
    pub fn readiness(&self) -> Readiness {
        self.readiness.clone()
    }

    pub fn shutdown_handle(&self) -> ShutdownHandle {
        self.shutdown.clone()
    }

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
    let lease = match SqliteStoreHandle::acquire_profile(config.store_dir()).await {
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
        owner_uid: endpoint.owner_uid,
        endpoint_path: endpoint.path().to_path_buf(),
    };
    shutdown_observer.publish_ready_if_idle(states);

    let mut connections = JoinSet::new();
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
                        let connection_context = context.clone();
                        let connection_drain = drain_receiver.clone();
                        connections.spawn(async move {
                            serve(stream, connection_context, connection_drain).await
                        });
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

    endpoint.close_listener();
    let (reason, mut forced) = match request {
        ShutdownRequest::Graceful { reason } => (reason, false),
        ShutdownRequest::Forced { reason } => (reason, true),
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

async fn shutdown_before_listener(
    config: &DaemonConfig,
    states: &StatePublisher,
    store: SqliteStoreHandle,
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
