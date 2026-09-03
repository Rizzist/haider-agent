//! The daemon phase machine and its shutdown controls.
//!
//! Every published phase change flows through [`StatePublisher::publish`],
//! which rejects any edge not in the transition diagram on [`DaemonState`] —
//! in every build, not only where assertions survive. `runtime.rs` decides
//! *when* to transition; this module owns *which* transitions exist.

use haider_rpc::LifecyclePhase;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod lifecycle_tests;

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
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

const STORE_OPEN: u8 = 1 << 0;
const RECOVERY_DONE: u8 = 1 << 1;
const PROVIDERS_LOADED: u8 = 1 << 2;
const SESSION_HUB_ACCEPTING_TURNS: u8 = 1 << 3;
const READY_PREREQUISITES: u8 =
    STORE_OPEN | RECOVERY_DONE | PROVIDERS_LOADED | SESSION_HUB_ACCEPTING_TURNS;

#[derive(Debug, Default)]
struct ReadinessFacts {
    prerequisites: AtomicU8,
    ready_since_unix_ms: AtomicU64,
}

/// One coherent read of the daemon readiness contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DaemonReadinessSnapshot {
    /// True only while every startup prerequisite is complete and the
    /// lifecycle is `Ready`.
    pub ready: bool,
    /// Unix epoch milliseconds at the successful Ready publication.
    pub ready_since_unix_ms: Option<u64>,
    /// Provider descriptors/factories are registered. This never claims an
    /// upstream provider connection; providers connect per request.
    pub providers_loaded: bool,
}

/// Read side of the daemon phase machine and its positive readiness latch.
#[derive(Debug, Clone)]
pub struct Readiness {
    receiver: watch::Receiver<DaemonState>,
    facts: Arc<ReadinessFacts>,
}

impl Readiness {
    /// The most recently published state.
    pub fn current(&self) -> DaemonState {
        self.receiver.borrow().clone()
    }

    /// Reads the one predicate used by status, `haider --ready`, and the
    /// launcher readiness channel.
    #[must_use]
    pub fn snapshot(&self) -> DaemonReadinessSnapshot {
        // Read Ready before the fact atomics so the watch publication is the
        // acquire edge for the publisher's preceding stores. Re-read it after
        // the facts so a concurrent drain cannot produce a positive snapshot.
        let state_before = self.current();
        let prerequisites = self.facts.prerequisites.load(Ordering::Acquire);
        let providers_loaded = prerequisites & PROVIDERS_LOADED != 0;
        let ready_since = self.facts.ready_since_unix_ms.load(Ordering::Acquire);
        let state_after = self.current();
        let ready = matches!(state_before, DaemonState::Ready)
            && matches!(state_after, DaemonState::Ready)
            && prerequisites & READY_PREREQUISITES == READY_PREREQUISITES
            && ready_since != 0;
        DaemonReadinessSnapshot {
            ready,
            ready_since_unix_ms: ready.then_some(ready_since),
            providers_loaded,
        }
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
    facts: Arc<ReadinessFacts>,
}

impl StatePublisher {
    pub(crate) fn channel() -> (Self, Readiness) {
        let (sender, receiver) = watch::channel(DaemonState::Starting);
        let facts = Arc::new(ReadinessFacts::default());
        (
            Self {
                sender,
                facts: Arc::clone(&facts),
            },
            Readiness { receiver, facts },
        )
    }

    pub(crate) fn readiness(&self) -> Readiness {
        Readiness {
            receiver: self.sender.subscribe(),
            facts: Arc::clone(&self.facts),
        }
    }

    pub(crate) fn mark_store_open(&self) {
        self.facts
            .prerequisites
            .fetch_or(STORE_OPEN, Ordering::Release);
    }

    pub(crate) fn mark_recovery_done(&self) {
        self.facts
            .prerequisites
            .fetch_or(RECOVERY_DONE, Ordering::Release);
    }

    pub(crate) fn mark_providers_loaded(&self) {
        self.facts
            .prerequisites
            .fetch_or(PROVIDERS_LOADED, Ordering::Release);
    }

    pub(crate) fn mark_session_hub_accepting_turns(&self) {
        self.facts
            .prerequisites
            .fetch_or(SESSION_HUB_ACCEPTING_TURNS, Ordering::Release);
    }

    /// Publishes a state, rejecting illegal transitions in EVERY build.
    ///
    /// The relation is contract for clients (they reason about phases through
    /// `Welcome`/readiness), so a bug must not be able to publish an edge that
    /// the diagram forbids just because assertions are compiled out. The
    /// rejected transition is dropped and reported, and the observable phase
    /// keeps its last legal value; `runtime.rs` reaches no such edge today,
    /// which is pinned by the transition-matrix test.
    pub(crate) fn publish(&self, state: DaemonState) -> bool {
        let prior = self.sender.borrow().clone();
        if !prior.can_transition_to(&state) {
            // Refusal, not an abort: the behaviour must be identical in every
            // build (a debug-only panic would make dev and release disagree
            // about a contract clients read), and it must be testable.
            eprintln!(
                "haider-daemon: refusing illegal lifecycle transition {prior:?} -> {state:?}"
            );
            return false;
        }
        if matches!(state, DaemonState::Ready) {
            let prerequisites = self.facts.prerequisites.load(Ordering::Acquire);
            if prerequisites & READY_PREREQUISITES != READY_PREREQUISITES {
                eprintln!("haider-daemon: refusing Ready before startup prerequisites completed");
                return false;
            }
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            let ready_since = u64::try_from(now_ms).unwrap_or(u64::MAX).max(1);
            self.facts
                .ready_since_unix_ms
                .store(ready_since, Ordering::Release);
        }
        self.sender.send_replace(state);
        true
    }
}

#[cfg(test)]
pub(crate) fn ready_for_tests() -> Readiness {
    let (publisher, readiness) = StatePublisher::channel();
    assert!(publisher.publish(DaemonState::Recovering));
    publisher.mark_store_open();
    publisher.mark_recovery_done();
    publisher.mark_providers_loaded();
    publisher.mark_session_hub_accepting_turns();
    assert!(publisher.publish(DaemonState::Ready));
    readiness
}

/// Latest shutdown demand as seen by the daemon task.
///
/// This is a watch value, not a queue: `Forced` overwrites `Graceful`, and
/// the daemon task only ever needs the strongest demand so far.
#[derive(Debug, Clone)]
pub(crate) enum ShutdownRequest {
    None,
    Graceful {
        reason: ShutdownReason,
    },
    /// The ephemeral launcher vanished. Start the ordinary graceful barrier
    /// only after every currently or subsequently attached client is gone.
    GracefulWhenIdle {
        reason: ShutdownReason,
    },
    /// The launching client vanished, but this daemon may serve new clients
    /// until it has had no attached client for the full bounded interval.
    GracefulAfterIdle {
        reason: ShutdownReason,
        idle_ttl: Duration,
    },
    Forced {
        reason: ShutdownReason,
    },
}

/// Typed origin retained by the shutdown control journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShutdownReason {
    /// The client process that spawned an ephemeral daemon disappeared.
    ClientVanished,
    /// Existing signal/RPC/internal reasons preserved verbatim.
    Message(String),
}

impl std::fmt::Display for ShutdownReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ClientVanished => formatter.write_str("spawning client vanished"),
            Self::Message(reason) => formatter.write_str(reason),
        }
    }
}

impl From<String> for ShutdownReason {
    fn from(reason: String) -> Self {
        Self::Message(reason)
    }
}

impl From<&str> for ShutdownReason {
    fn from(reason: &str) -> Self {
        Self::Message(reason.to_owned())
    }
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
    /// Sticky proof that a stop caller needs this drain's completion receipt.
    operator_stop: AtomicBool,
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
            operator_stop: AtomicBool::new(false),
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
        let reason = ShutdownReason::Message(reason.into());
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

    /// Starts or advances to an immediate graceful drain without ever forcing.
    ///
    /// Operator RPCs are idempotent: a daemon may already carry an idle or
    /// linger request from its launcher, and independent stop callers may race.
    /// Only repeated process signals use [`ShutdownHandle::request`] to select
    /// the forced path.
    pub fn request_graceful(&self) {
        let reason = ShutdownReason::Message("authenticated daemon.shutdown RPC".into());
        let _transition = lock_transition(&self.inner);
        self.inner.operator_stop.store(true, Ordering::Release);
        let current = self.inner.sender.borrow().clone();
        match current {
            ShutdownRequest::None => {
                self.inner.requests.store(1, Ordering::Release);
                self.inner
                    .sender
                    .send_replace(ShutdownRequest::Graceful { reason });
            }
            ShutdownRequest::GracefulWhenIdle { .. }
            | ShutdownRequest::GracefulAfterIdle { .. } => {
                self.inner
                    .sender
                    .send_replace(ShutdownRequest::Graceful { reason });
            }
            ShutdownRequest::Graceful { .. } | ShutdownRequest::Forced { .. } => {}
        }
    }

    /// Registers a stop caller before its Welcome is published. The sticky bit
    /// lets that caller observe a drain which another source already started.
    pub(crate) fn observe_operator_stop(&self) {
        self.inner.operator_stop.store(true, Ordering::Release);
    }

    /// Records an ephemeral launcher death without disconnecting unrelated
    /// live clients. Returns whether this installed the first shutdown demand.
    pub(crate) fn request_when_idle(&self, reason: ShutdownReason) -> bool {
        let _transition = lock_transition(&self.inner);
        if self.inner.requests.load(Ordering::Acquire) != 0 {
            return false;
        }
        self.inner.requests.store(1, Ordering::Release);
        self.inner
            .sender
            .send_replace(ShutdownRequest::GracefulWhenIdle { reason });
        true
    }

    /// Records a bounded linger after launcher death. The daemon runtime owns
    /// the timer, so abrupt launcher termination cannot cancel or bypass it.
    pub(crate) fn request_after_idle(&self, reason: ShutdownReason, idle_ttl: Duration) -> bool {
        let _transition = lock_transition(&self.inner);
        if self.inner.requests.load(Ordering::Acquire) != 0 {
            return false;
        }
        self.inner.requests.store(1, Ordering::Release);
        self.inner
            .sender
            .send_replace(ShutdownRequest::GracefulAfterIdle { reason, idle_ttl });
        true
    }
}

impl ShutdownObserver {
    /// Returns whether a stop caller registered for this drain's completion.
    pub(crate) fn operator_stop_requested(&self) -> bool {
        self.inner.operator_stop.load(Ordering::Acquire)
    }

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
        states.publish(DaemonState::Ready)
    }
}

/// The guarded section never panics, so a poisoned mutex is still consistent.
fn lock_transition(inner: &ShutdownInner) -> MutexGuard<'_, ()> {
    match inner.transition.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}
