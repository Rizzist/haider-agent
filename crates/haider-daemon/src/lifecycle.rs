use haider_rpc::LifecyclePhase;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, MutexGuard};
use tokio::sync::watch;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonState {
    Starting,
    Recovering,
    Ready,
    Draining {
        reason: String,
        deadline_unix_ms: u64,
    },
    Failed {
        message: String,
    },
    Stopped,
}

impl DaemonState {
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
}

#[derive(Debug, Clone)]
pub struct Readiness {
    receiver: watch::Receiver<DaemonState>,
}

impl Readiness {
    pub fn current(&self) -> DaemonState {
        self.receiver.borrow().clone()
    }

    /// Waits for an actual state publication; callers need no timing sleeps.
    pub async fn changed(&mut self) -> Option<DaemonState> {
        self.receiver.changed().await.ok().map(|()| self.current())
    }
}

pub(crate) struct StatePublisher {
    sender: watch::Sender<DaemonState>,
}

impl StatePublisher {
    pub(crate) fn channel() -> (Self, Readiness) {
        let (sender, receiver) = watch::channel(DaemonState::Starting);
        (Self { sender }, Readiness { receiver })
    }

    pub(crate) fn publish(&self, state: DaemonState) {
        self.sender.send_replace(state);
    }
}

#[derive(Debug, Clone)]
pub(crate) enum ShutdownRequest {
    None,
    Graceful { reason: String },
    Forced { reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownDisposition {
    DrainStarted,
    Forced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownOutcome {
    Graceful,
    Forced,
}

struct ShutdownInner {
    requests: AtomicU8,
    transition: Mutex<()>,
    sender: watch::Sender<ShutdownRequest>,
}

#[derive(Clone)]
pub(crate) struct ShutdownObserver {
    inner: Arc<ShutdownInner>,
}

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
    pub(crate) fn publish_ready_if_idle(&self, states: &StatePublisher) -> bool {
        let _transition = lock_transition(&self.inner);
        if self.inner.requests.load(Ordering::Acquire) != 0 {
            return false;
        }
        states.publish(DaemonState::Ready);
        true
    }
}

fn lock_transition(inner: &ShutdownInner) -> MutexGuard<'_, ()> {
    match inner.transition.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}
