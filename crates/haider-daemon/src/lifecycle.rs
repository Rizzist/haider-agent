//! The daemon phase machine and its shutdown controls.
//!
//! Every published phase change flows through [`StatePublisher::publish`],
//! which rejects (loudly, in debug builds) any edge not in the transition
//! diagram on [`DaemonState`]. `runtime.rs` decides *when* to transition;
//! this module owns *which* transitions exist.

use haider_rpc::LifecyclePhase;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, MutexGuard};
use tokio::sync::watch;

/// Observable daemon lifecycle phase (d1 report R16/R17 daemon half of R4).
///
/// Legal transitions — everything else is a bug and is rejected by the
/// crate-internal publisher:
///
/// ```text
/// Starting ──► Recovering ──► Ready ──► Draining ──► Stopped
///     │             │           │           │
///     └──►──────────┴───────────┴───────────┴──► Failed
///     └──►──────────► Draining (shutdown before recovery finished)
///                └──►─► Draining (shutdown before listener bound)
/// ```
///
/// `Stopped` and `Failed` are terminal. `Ready` is reached only after the
/// reconcile-before-ready gate; `Draining` is entered exactly once, on the
/// first shutdown request (graceful or forced).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonState {
    /// Config validated, profile lock not yet held.
    Starting,
    /// Lock held; store opening, daemon generation bump, and C4a effect
    /// reconciliation are in flight. Listener is not bound yet.
    Recovering,
    /// Reconciliation complete, listener bound, handshakes are served.
    Ready,
    /// Drain barrier active: no new connections, `ServerDraining` sent,
    /// bounded completion window running (R17).
    Draining {
        reason: String,
        deadline_unix_ms: u64,
    },
    /// Terminal: startup or shutdown hit an error. The profile lock has been
    /// (or is being) released; the joined task carries the typed error.
    Failed { message: String },
    /// Terminal: orderly stop; socket removed, lock released.
    Stopped,
}

impl DaemonState {
    /// The wire-visible phase advertised in `Welcome` (haider-rpc).
    pub fn phase(&self) -> LifecyclePhase {
        match self {
            Self::Starting => LifecyclePhase::Starting,
            Self::Recovering => LifecyclePhase::Recovering,
            Self::Ready => LifecyclePhase::Ready,
            Self::Draining { .. } => LifecyclePhase::Draining,
            Self::Failed { .. } => LifecyclePhase::Failed,
            Self::Stopped => LifecyclePhase::Stopped,
        }
    }

    /// The transition relation of the diagram above, in one place.
    ///
    /// Any non-terminal state may fail; forward progress is otherwise strictly
    /// Starting → Recovering → Ready → Draining → Stopped, with Draining
    /// reachable early when shutdown pre-empts startup.
    pub fn can_transition_to(&self, next: &DaemonState) -> bool {
        matches!(
            (self, next),
            (Self::Starting, Self::Recovering)
                | (Self::Starting | Self::Recovering, Self::Draining { .. })
                | (Self::Recovering, Self::Ready)
                | (Self::Ready, Self::Draining { .. })
                | (Self::Draining { .. }, Self::Stopped)
                | (
                    Self::Starting | Self::Recovering | Self::Ready | Self::Draining { .. },
                    Self::Failed { .. },
                )
        )
    }
}

/// Read side of the daemon phase machine (the R4 readiness handshake's
/// in-process form; the wire form is `Welcome.lifecycle_phase`).
#[derive(Debug, Clone)]
pub struct Readiness {
    receiver: watch::Receiver<DaemonState>,
}

impl Readiness {
    /// The most recently published state.
    pub fn current(&self) -> DaemonState {
        self.receiver.borrow().clone()
    }

    /// Waits for an actual state publication; callers need no timing sleeps.
    ///
    /// Returns `None` once the daemon task has finished and dropped its
    /// publisher; [`Readiness::current`] then holds the terminal state.
    pub async fn changed(&mut self) -> Option<DaemonState> {
        self.receiver.changed().await.ok().map(|()| self.current())
    }
}

/// Sole writer of [`DaemonState`]; owned by the daemon task in `runtime.rs`.
pub(crate) struct StatePublisher {
    sender: watch::Sender<DaemonState>,
}

impl StatePublisher {
    pub(crate) fn channel() -> (Self, Readiness) {
        let (sender, receiver) = watch::channel(DaemonState::Starting);
        (Self { sender }, Readiness { receiver })
    }

    /// Publishes a state, rejecting illegal transitions loudly in debug
    /// builds (release builds publish unconditionally rather than wedge).
    pub(crate) fn publish(&self, state: DaemonState) {
        let prior = self.sender.send_replace(state.clone());
        debug_assert!(
            prior.can_transition_to(&state),
            "illegal lifecycle transition {prior:?} -> {state:?}"
        );
    }
}

/// Latest shutdown demand as seen by the daemon task.
///
/// This is a watch value, not a queue: `Forced` overwrites `Graceful`, and
/// the daemon task only ever needs the strongest demand so far.
#[derive(Debug, Clone)]
pub(crate) enum ShutdownRequest {
    None,
    Graceful { reason: String },
    Forced { reason: String },
}

/// What one [`ShutdownHandle::request`] call did (first vs. later request).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownDisposition {
    /// This was the first request; the drain barrier (R17) starts.
    DrainStarted,
    /// A drain was already underway; this request forces termination.
    Forced,
}

/// How the daemon task ended, reported by `DaemonTask::join`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownOutcome {
    /// Drain completed within its window: connections notified and closed,
    /// store flushed, socket removed, lock released last.
    Graceful,
    /// The forced path ran: pending connections aborted, flush errors
    /// ignored. Recovery is the next daemon generation's job (R17).
    Forced,
}

struct ShutdownInner {
    /// Count of shutdown requests so far; `> 0` means a drain has started.
    requests: AtomicU8,
    /// Serializes shutdown requests against the Ready publication so that
    /// `Ready` can never be advertised at or after the first request.
    transition: Mutex<()>,
    sender: watch::Sender<ShutdownRequest>,
}

/// Crate-internal view used by the daemon task to publish `Ready` only if no
/// shutdown request has arrived (the other side of the `transition` mutex).
#[derive(Clone)]
pub(crate) struct ShutdownObserver {
    inner: Arc<ShutdownInner>,
}

/// External shutdown control for one daemon task.
///
/// Law (R17): the first [`ShutdownHandle::request`] starts the drain barrier;
/// every later request selects the forced-termination path. Signal handlers
/// (`run_with_signals`) call this exactly once per delivered signal.
#[derive(Clone)]
pub struct ShutdownHandle {
    inner: Arc<ShutdownInner>,
}

impl std::fmt::Debug for ShutdownHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ShutdownHandle")
            .field("requests", &self.inner.requests.load(Ordering::Acquire))
            .finish()
    }
}

impl ShutdownHandle {
    pub(crate) fn channel() -> (Self, watch::Receiver<ShutdownRequest>, ShutdownObserver) {
        let (sender, receiver) = watch::channel(ShutdownRequest::None);
        let inner = Arc::new(ShutdownInner {
            requests: AtomicU8::new(0),
            transition: Mutex::new(()),
            sender,
        });
        let handle = Self {
            inner: Arc::clone(&inner),
        };
        (handle, receiver, ShutdownObserver { inner })
    }

    /// First request starts draining; every later request forces termination.
    pub fn request(&self, reason: impl Into<String>) -> ShutdownDisposition {
        let reason = reason.into();
        let _transition = lock_transition(&self.inner);
        let prior = self.inner.requests.load(Ordering::Acquire);
        self.inner
            .requests
            .store(prior.saturating_add(1), Ordering::Release);
        if prior == 0 {
            self.inner
                .sender
                .send_replace(ShutdownRequest::Graceful { reason });
            ShutdownDisposition::DrainStarted
        } else {
            self.inner
                .sender
                .send_replace(ShutdownRequest::Forced { reason });
            ShutdownDisposition::Forced
        }
    }
}

impl ShutdownObserver {
    /// Publishes `Ready` unless a shutdown request already arrived.
    ///
    /// Holding the transition mutex across the check-and-publish closes the
    /// race with a concurrent first [`ShutdownHandle::request`]: a client can
    /// never observe `Ready` after the daemon has committed to draining.
    pub(crate) fn publish_ready_if_idle(&self, states: &StatePublisher) -> bool {
        let _transition = lock_transition(&self.inner);
        if self.inner.requests.load(Ordering::Acquire) != 0 {
            return false;
        }
        states.publish(DaemonState::Ready);
        true
    }
}

/// The guarded section never panics, so a poisoned mutex is still consistent.
fn lock_transition(inner: &ShutdownInner) -> MutexGuard<'_, ()> {
    match inner.transition.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}
