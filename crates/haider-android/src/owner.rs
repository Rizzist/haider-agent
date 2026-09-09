//! The native owner is shared with host tests; JNI only supplies Context/inputs.
use crate::completion::CompletionReceipt;
use crate::contract::{NativeStatus as Status, Observation, Paths};
use haider_daemon::{DaemonState, DaemonTask, ShutdownOutcome};
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::watch;

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct Initialized<C> {
    context: Arc<C>,
    paths: Paths,
    running: Option<Arc<Running>>,
}

pub(crate) struct Host<C> {
    initialized: Mutex<Option<Initialized<C>>>,
    path_identity: OnceLock<Paths>,
    // Observe/stop do not contend with initialization or proxy Binder calls.
    live: Mutex<Option<Arc<Running>>>,
    terminal: Mutex<Observation>,
}

impl<C: Send + Sync + 'static> Host<C> {
    pub(crate) fn new() -> Self {
        Self {
            initialized: Mutex::new(None),
            path_identity: OnceLock::new(),
            live: Mutex::new(None),
            terminal: Mutex::new(Observation::phase("Stopped", 0)),
        }
    }

    /// The adapter checks an existing context or installs a new owned context.
    pub(crate) fn initialize(
        &self,
        paths: Paths,
        install: impl FnOnce(Option<&C>) -> Result<Option<C>, Status>,
    ) -> Result<Status, Status> {
        let mut slot = lock(&self.initialized);
        if self
            .path_identity
            .get()
            .is_some_and(|previous| previous != &paths)
        {
            return Err(Status::BadPaths);
        }
        if let Some(previous) = slot.as_ref() {
            install(Some(&previous.context))?;
            return Ok(Status::Ok);
        }
        let context = install(None)?.ok_or(Status::Internal)?;
        let _ = self.path_identity.set(paths.clone());
        *slot = Some(Initialized {
            context: Arc::new(context),
            paths,
            running: None,
        });
        Ok(Status::Ok)
    }

    pub(crate) fn with_context<T>(
        &self,
        operation: impl FnOnce(&C) -> Result<T, Status>,
    ) -> Result<T, Status> {
        let slot = lock(&self.initialized);
        operation(&slot.as_ref().ok_or(Status::NotInitialized)?.context)
    }

    pub(crate) fn is_running(&self) -> bool {
        lock(&self.live)
            .as_ref()
            .is_some_and(|r| r.completion.get().is_none())
    }

    pub(crate) fn start(
        self: &Arc<Self>,
        run: impl FnOnce(&Running, watch::Receiver<u8>, &Paths) -> Status + Send + 'static,
    ) -> Result<Status, Status> {
        let mut slot = lock(&self.initialized);
        let initialized = slot.as_mut().ok_or(Status::NotInitialized)?;
        if initialized
            .running
            .as_ref()
            .is_some_and(|r| r.completion.get().is_none())
        {
            return Err(Status::AlreadyRunning);
        }
        let (stop, receiver) = watch::channel(0);
        let running = Arc::new(Running {
            stop,
            probe: Mutex::new(None),
            completion: CompletionReceipt::new(Observation::phase("Starting", 0)),
        });
        let owner = Arc::clone(&running);
        let context = Arc::clone(&initialized.context);
        let paths = initialized.paths.clone();
        let host = Arc::clone(self);
        std::thread::Builder::new()
            .name("haider-android-owner".into())
            .stack_size(8 * 1024 * 1024)
            .spawn(move || {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    let status = run(&owner, receiver, &paths);
                    // run has destroyed its runtime. Unwinding run also drops
                    // its runtime before this captured context can be dropped.
                    drop(context);
                    let generation = owner.observe().daemon_generation;
                    *lock(&owner.probe) = None;
                    let observation = if matches!(status, Status::Ok | Status::ShutdownForced) {
                        Observation::phase("Stopped", generation)
                    } else {
                        Observation::failed(status, generation)
                    };
                    let _ = crate::logging::record(&paths.logs_dir, &observation);
                    status
                }));
                let status = result.unwrap_or(Status::Internal);
                *lock(&owner.probe) = None;
                owner.completion.finish(status, |terminal| {
                    *lock(&host.terminal) = terminal.clone();
                });
            })
            .map_err(|_| Status::Internal)?;
        *lock(&self.live) = Some(Arc::clone(&running));
        initialized.running = Some(running);
        Ok(Status::Ok)
    }

    pub(crate) fn observe(&self) -> Observation {
        let running = lock(&self.live).clone();
        running.map_or_else(|| lock(&self.terminal).clone(), |r| r.observe())
    }

    pub(crate) fn fail_boundary(&self) {
        if let Some(running) = lock(&self.live).clone() {
            running.completion.fail(Status::Internal);
        }
    }

    pub(crate) fn shutdown(&self, forced: bool, budget: Duration) -> Status {
        let running = lock(&self.live).clone();
        running.map_or(Status::Ok, |r| r.shutdown(forced, budget))
    }

    /// False means ownership is retained, including the application context.
    pub(crate) fn release(&self, budget: Duration) -> bool {
        let running = lock(&self.live).clone();
        if let Some(running) = running {
            if running.shutdown(true, budget) == Status::ShutdownTimeout {
                return false;
            }
            let mut slot = lock(&self.initialized);
            if slot
                .as_ref()
                .and_then(|d| d.running.as_ref())
                .is_some_and(|current| Arc::ptr_eq(current, &running))
            {
                *lock(&self.live) = None;
                slot.take();
            }
        } else {
            let mut slot = lock(&self.initialized);
            if slot.as_ref().is_some_and(|d| d.running.is_none()) {
                slot.take();
            }
        }
        true
    }
}

struct Probe {
    readiness: haider_daemon::Readiness,
    diagnostics: haider_daemon::DaemonTaskDiagnostics,
}

pub(crate) struct Running {
    stop: watch::Sender<u8>,
    probe: Mutex<Option<Probe>>,
    completion: CompletionReceipt,
}

impl Running {
    fn observe(&self) -> Observation {
        self.completion.observe(|last| {
            if let Some(probe) = lock(&self.probe).as_ref() {
                let facts = probe.readiness.snapshot();
                let diagnostics = probe.diagnostics.snapshot();
                let generation = diagnostics
                    .bootstrap
                    .as_ref()
                    .map_or(0, |b| b.daemon_generation);
                let phase = match diagnostics.state {
                    DaemonState::Starting => "Starting",
                    DaemonState::Recovering => "Recovering",
                    DaemonState::Ready if facts.ready => "Ready",
                    DaemonState::Ready => "Recovering",
                    DaemonState::Draining { .. } | DaemonState::Stopped => "Draining",
                    DaemonState::Failed { .. } => "Failed",
                };
                *last = Observation::phase(phase, generation);
                if phase == "Ready"
                    && let Some(bootstrap) = diagnostics.bootstrap
                {
                    last.ready_since_unix_ms = facts.ready_since_unix_ms;
                    last.endpoint_path = Some(bootstrap.endpoint_path);
                }
            }
        })
    }

    fn shutdown(&self, forced: bool, deadline: Duration) -> Status {
        self.stop.send_if_modified(|mode| {
            let next = if forced { 2 } else { 1 };
            if *mode < next {
                *mode = next;
                true
            } else {
                false
            }
        });
        self.completion
            .wait_for(deadline)
            .unwrap_or(Status::ShutdownTimeout)
    }
}

/// Uses exactly one consuming daemon join, and destroys the runtime (including
/// outstanding blocking readers) before returning to the native owner thread.
pub(crate) fn run_daemon(
    owner: &Running,
    mut stop: watch::Receiver<u8>,
    bootstrap: impl Future<Output = Result<DaemonTask, Status>>,
    paths: &Paths,
) -> Status {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(4)
        .thread_stack_size(8 * 1024 * 1024)
        .thread_name("haider-android")
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => return Status::Internal,
    };
    let status = runtime.block_on(async {
        let task = match bootstrap.await { Ok(task) => task, Err(status) => return status };
        let shutdown = task.shutdown_handle();
        *lock(&owner.probe) = Some(Probe { readiness: task.readiness(), diagnostics: task.diagnostics() });
        let join = task.join();
        tokio::pin!(join);
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        let mut logged_phase = "";
        loop {
            let mode = *stop.borrow_and_update();
            if mode > 0 { shutdown.request_graceful(); }
            if mode > 1 { shutdown.request("android-forced"); }
            tokio::select! {
                _ = tick.tick() => {
                    let observation = owner.observe();
                    if observation.phase != logged_phase {
                        let _ = crate::logging::record(&paths.logs_dir, &observation);
                        logged_phase = observation.phase;
                    }
                }
                result = &mut join => break match result {
                    Ok(ShutdownOutcome::Graceful) => Status::Ok,
                    Ok(ShutdownOutcome::Forced) => Status::ShutdownForced,
                    Err(haider_daemon::DaemonError::Store(_)) => Status::StoreRecoveryFailed,
                    Err(haider_daemon::DaemonError::AlreadyRunning { .. }) => Status::AlreadyRunning,
                    Err(_) => Status::Internal,
                },
                changed = stop.changed() => if changed.is_err() {
                    shutdown.request_graceful();
                    shutdown.request("android-owner-lost");
                }
            }
        }
    });
    drop(runtime);
    status
}

#[cfg(test)]
#[path = "owner_tests.rs"]
mod tests;
