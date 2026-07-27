//! Session-scoped actors, attachment replay, and durable menu arbitration.
//!
//! One actor owns the serialized command order for each live session. That
//! order is the proof boundary for the laws below.
//!
//! # Laws stated here (every other comment refers back to this list)
//!
//! - **INVARIANT 1 — persist before publish (§5.5).** An envelope is
//!   published to attachments (and the harness) only after its store append
//!   has returned. Holds by code shape in [`run_session_actor`]: the store
//!   call is awaited and `publish` is a synchronous call later in the same
//!   command arm.
//! - **INVARIANT 2 — register receiver + observe head `H` in one serialized
//!   step (§5.5).** Receiver insertion and committed-head capture are
//!   adjacent synchronous statements inside the actor's `Register` arm; no
//!   await or yield exists between them, so no append can interleave. Holds
//!   by code shape, and the forced-boundary tests assert it.
//! - **The store is the lag buffer (R12).** Replay pages the store; live
//!   delivery crosses a bounded per-attachment catch-up channel — bounded in
//!   frames AND estimated bytes ([`publish`]) — and the bounded connection
//!   outbox, while replay store pages are byte-budgeted
//!   (`Store::read_page`). On any overflow the attachment is lagged and
//!   detached, then resumes FROM THE STORE after its applied cursor. No
//!   unbounded queue ever buffers history for a slow client.
//! - **Replay is paced by real writer state.** Delivery bursts never exceed
//!   the sink's available quota and wait on its drain-progress signal, so a
//!   reading client is never lagged just because a page outran its writer;
//!   an actual refusal or lag-under-stall still detaches immediately
//!   ([`acquire_send_capacity`] states the law).
//! - **Attachment admission is capped** per connection and hub-wide, refused
//!   BEFORE any actor or channel work with the correlated, retryable
//!   `overloaded` error ([`SessionHubConfig`] and
//!   `SessionHub::reserve_attachment_slot`).
//!
//! Two laws this module obeys but does not own:
//!
//! - menu arbitration (first committed answer wins) is stated on
//!   `haider_store::Store::resolve_menu`;
//! - the fair-scheduling policy is stated on `connection.rs`'s
//!   `OutboundLane`.
//!
//! # Mechanism
//!
//! Replays are separate cancellable tasks. They page `(after_seq, H]`, emit
//! `AttachCaughtUp(H)`, drain the already-registered bounded receiver for
//! `seq > H`, then stay live ([`run_replay`] documents the phases).
//!
//! # W3c/W3d seams
//!
//! A real client reaches sessions only through this surface: after handshake
//! negotiation, [`SessionHub::open_connection`] with the transport's
//! [`FrameSink`], then [`HubConnection::request`] and
//! [`HubConnection::menu_answer`]. The CLI `haider attach` and the TUI's
//! live-attach path (W3c) and the localhost WebSocket layer (W3d) add
//! transports over this seam, not new semantics. In-process workers join
//! through [`SessionHub::register_harness`] and must use the hub as their
//! `StoreHandle` (see [`SessionHub::append`]).

use crate::DaemonError;
use async_trait::async_trait;
use haider_core::{
    HarnessHandle, MenuResolutionCommand, MenuResolutionOutcome, SqliteStoreHandle, StoreHandle,
};
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::ids::{DeviceId, SessionId};
use haider_protocol::menu::{AnswerVia, MenuAnswer as DurableMenuAnswer};
use haider_rpc::{
    AttachMode, AttachState, AttachmentId, Capability, CapabilitySet, CommandId,
    ERROR_CODE_ALREADY_RESOLVED, ERROR_CODE_CAPABILITY_DENIED, ERROR_CODE_CURSOR_AHEAD,
    ERROR_CODE_DRAINING, ERROR_CODE_INVALID_ARGUMENT, ERROR_CODE_INVALID_CURSOR,
    ERROR_CODE_NOT_FOUND, ERROR_CODE_OVERLOADED, ERROR_CODE_STALE_GENERATION, ErrorData, MenuInput,
    ProtocolError, RequestBody, RequestId, ResponseBody, SeqRange, SessionReadResult,
    SessionSummary, WireFrame,
};
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

const REPLAY_PAGE_SIZE: usize = 256;
const MAX_LIST_PAGE: usize = 100;
const MAX_READ_ENVELOPES: usize = 1_024;

// ────────────────── configuration, sink seam, and observer ──────────────────

/// Bounds for session-actor, attachment-admission, and catch-up traffic.
#[derive(Debug, Clone, Copy)]
pub struct SessionHubConfig {
    /// Commands waiting at one live session actor.
    pub actor_command_capacity: usize,
    /// Commits after `H` retained while one attachment replays (frame count;
    /// `catch_up_byte_budget` bounds the same channel in bytes).
    pub catch_up_capacity: usize,
    /// Estimated bytes of committed envelopes one attachment's catch-up
    /// channel may retain. Overflow takes the exact same nonblocking
    /// lag-then-store-resume transition as a full frame count. A single
    /// envelope is always admitted into an EMPTY channel even when it alone
    /// exceeds this budget, so an oversized envelope can never wedge an
    /// attachment in a lag loop.
    pub catch_up_byte_budget: usize,
    /// Stored-JSON bytes one replay store page may materialize
    /// (`Store::read_page`); the page ends early when the budget fills and
    /// the next page resumes from the last delivered sequence.
    pub replay_page_byte_budget: usize,
    /// Attachments one connection may hold concurrently. The N+1th
    /// `SessionAttach` is rejected with the correlated, retryable
    /// `overloaded` error before any actor or channel work happens.
    pub max_attachments_per_connection: usize,
    /// Attachments the whole hub may hold concurrently, independent of the
    /// per-connection cap; same rejection shape.
    pub max_attachments: usize,
}

impl Default for SessionHubConfig {
    fn default() -> Self {
        Self {
            actor_command_capacity: 64,
            catch_up_capacity: 64,
            catch_up_byte_budget: 8 * 1024 * 1024,
            replay_page_byte_budget: 1024 * 1024,
            max_attachments_per_connection: 16,
            max_attachments: 256,
        }
    }
}

/// A nonblocking destination for frames produced by one hub connection.
///
/// The production implementation is the connection's bounded fair outbox.
/// Tests may use a deterministic sink to stop at replay boundaries.
pub trait FrameSink: Send + Sync {
    /// Admits one complete frame without waiting on a socket.
    fn try_send(&self, frame: WireFrame) -> Result<(), FrameSendError>;

    /// Purges queued traffic for a detached attachment when the sink supports
    /// keyed lanes. The default is suitable for sinks without staging queues.
    fn purge_attachment(&self, _attachment_id: &AttachmentId) {}

    /// Frames this attachment's lane can currently admit without refusal
    /// (frame-count dimension only — a byte-budget refusal by `try_send`
    /// remains an ACTUAL refusal). Replay pacing sizes its bursts by this
    /// value so a reading client is never lagged just because a page was
    /// larger than the lane. Sinks without admission pressure keep the
    /// unlimited default.
    fn capacity_for(&self, _attachment_id: &AttachmentId) -> usize {
        usize::MAX
    }

    /// Real-state drain signal: the receiver observes a change whenever
    /// queued frames leave the sink, so replay can await writer progress
    /// instead of sleeping or guessing. `None` (the default) means the sink
    /// never exerts admission pressure and `try_send` is the only arbiter.
    fn drain_progress(&self) -> Option<watch::Receiver<u64>> {
        None
    }
}

/// A bounded sink refused a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameSendError;

impl fmt::Display for FrameSendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("bounded connection outbox refused a frame")
    }
}

impl std::error::Error for FrameSendError {}

/// Semantic boundary emitted by the hub's optional observer.
///
/// Production uses the no-op observer. The observer contract is nonblocking;
/// deterministic tests may deliberately gate a replay task or actor turn to
/// force every §5.5 interleaving.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum HubObservation {
    AppendEnqueued {
        session_id: SessionId,
    },
    ShutdownGuarded,
    ReceiverRegistered {
        attachment_id: AttachmentId,
    },
    HeadCaptured {
        attachment_id: AttachmentId,
        head: u64,
    },
    Persisted {
        session_id: SessionId,
        through_seq: u64,
    },
    Published {
        session_id: SessionId,
        through_seq: u64,
    },
    ReplayEvent {
        attachment_id: AttachmentId,
        seq: u64,
    },
    BeforeCaughtUp {
        attachment_id: AttachmentId,
        through_seq: u64,
    },
    CaughtUp {
        attachment_id: AttachmentId,
        through_seq: u64,
    },
    BufferedEvent {
        attachment_id: AttachmentId,
        seq: u64,
    },
    LiveEvent {
        attachment_id: AttachmentId,
        seq: u64,
    },
    BeforeEvent {
        attachment_id: AttachmentId,
        seq: u64,
    },
}

/// Optional boundary observer. Implementations must return promptly outside
/// deterministic tests.
pub trait SessionHubObserver: Send + Sync {
    fn observe(&self, observation: HubObservation);
}

#[derive(Debug)]
struct NoopObserver;

impl SessionHubObserver for NoopObserver {
    fn observe(&self, _observation: HubObservation) {}
}

// ──────────────────── hub state, task ownership, errors ─────────────────────

/// Cloneable owner of every live session actor and replay task.
#[derive(Clone)]
pub struct SessionHub {
    inner: Arc<HubInner>,
}

struct HubInner {
    store: SqliteStoreHandle,
    config: SessionHubConfig,
    observer: Arc<dyn SessionHubObserver>,
    actors: Mutex<HashMap<SessionId, SessionActorHandle>>,
    actor_tasks: Mutex<Vec<JoinHandle<()>>>,
    replay_tasks: Mutex<Vec<JoinHandle<()>>>,
    attachments: Mutex<HashMap<AttachmentId, AttachmentOwner>>,
    /// Admission ledger for the per-connection and global attachment caps.
    /// A slot is reserved BEFORE any actor or channel work and released when
    /// registration fails or `take_attachment` removes the owner, so every
    /// admitted attachment's whole resource footprint sits behind one
    /// reservation.
    attachment_slots: Mutex<AttachmentSlots>,
    draining: AtomicBool,
    device_id: DeviceId,
}

#[derive(Default)]
struct AttachmentSlots {
    total: usize,
    per_connection: HashMap<String, usize>,
}

/// Join handles that abort every still-owned task if the enclosing shutdown
/// future is itself cancelled at the global drain deadline.
struct OwnedTasks {
    handles: Vec<JoinHandle<()>>,
}

impl OwnedTasks {
    fn new(handles: Vec<JoinHandle<()>>) -> Self {
        Self { handles }
    }

    fn abort_all(&self) {
        for handle in &self.handles {
            handle.abort();
        }
    }

    async fn join_all(&mut self) {
        while let Some(handle) = self.handles.last_mut() {
            let _ = handle.await;
            self.handles.pop();
        }
    }
}

impl Drop for OwnedTasks {
    fn drop(&mut self) {
        self.abort_all();
    }
}

#[derive(Clone)]
struct SessionActorHandle {
    commands: mpsc::Sender<ActorCommand>,
    stopping: Arc<AtomicBool>,
}

/// Hub-level attachment metadata (§5.6): who owns the attachment and with
/// which mode. Policy checks (e.g. the menu control-attachment requirement)
/// read this map; the actor keeps only delivery state ([`ActorAttachment`]).
struct AttachmentOwner {
    connection_id: String,
    session_id: SessionId,
    mode: AttachMode,
    actor: SessionActorHandle,
    cancel: watch::Sender<bool>,
}

/// One committed envelope in flight on a catch-up channel, carrying the
/// weight it was charged so receive-side credit is exactly symmetric.
struct QueuedEnvelope {
    weight: usize,
    envelope: RawEnvelope,
}

/// Actor-side delivery state for one registered attachment.
struct ActorAttachment {
    events: mpsc::Sender<QueuedEnvelope>,
    lagged: watch::Sender<Option<u64>>,
    /// Estimated bytes currently queued on `events` — charged by the actor
    /// before enqueue, credited by the replay task on receive, reset to zero
    /// when re-registration replaces the channel wholesale.
    queued_bytes: Arc<AtomicUsize>,
    last_buffered_seq: u64,
    active: bool,
}

struct Registration {
    attachment_id: AttachmentId,
    attach_state: AttachState,
    actor: SessionActorHandle,
    events: mpsc::Receiver<QueuedEnvelope>,
    lagged: watch::Receiver<Option<u64>>,
    /// Shared with [`ActorAttachment::queued_bytes`]; the replay task credits
    /// it as envelopes leave the channel.
    catch_up_bytes: Arc<AtomicUsize>,
}

enum RegisterResult {
    Registered(Registration),
    CursorAhead {
        requested: u64,
        head: u64,
    },
    /// An admission cap refused the attachment before any resources existed.
    Overloaded {
        message: String,
    },
}

enum ActorRegisterResult {
    Registered(AttachState),
    CursorAhead { requested: u64, head: u64 },
}

enum ActorCommand {
    Append {
        envelopes: Vec<RawEnvelope>,
        completed: oneshot::Sender<Result<Vec<RawEnvelope>, HaiderError>>,
    },
    Register {
        attachment_id: AttachmentId,
        after_seq: u64,
        events: mpsc::Sender<QueuedEnvelope>,
        lagged: watch::Sender<Option<u64>>,
        queued_bytes: Arc<AtomicUsize>,
        completed: oneshot::Sender<ActorRegisterResult>,
    },
    Reregister {
        attachment_id: AttachmentId,
        events: mpsc::Sender<QueuedEnvelope>,
        lagged: watch::Sender<Option<u64>>,
        completed: oneshot::Sender<Option<u64>>,
    },
    Detach {
        attachment_id: AttachmentId,
    },
    MenuAnswer {
        command: MenuResolutionCommand,
        completed: oneshot::Sender<Result<MenuResolutionOutcome, HaiderError>>,
    },
    RegisterHarness {
        harness: HarnessHandle,
    },
    Stop,
}

/// One negotiated connection's authorization and attachment ownership.
pub struct HubConnection {
    hub: SessionHub,
    connection_id: String,
    capabilities: CapabilitySet,
    sink: Arc<dyn FrameSink>,
    closed: AtomicBool,
}

/// Infrastructure failure while routing a frame.
#[derive(Debug)]
pub enum SessionHubError {
    Closed,
    Store(HaiderError),
    Delivery,
    Task(String),
    InvalidConfig(String),
}

impl fmt::Display for SessionHubError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => formatter.write_str("session hub is draining or closed"),
            Self::Store(error) => write!(formatter, "session store failed: {error:?}"),
            Self::Delivery => formatter.write_str("connection outbox refused a frame"),
            Self::Task(message) | Self::InvalidConfig(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for SessionHubError {}

impl From<HaiderError> for SessionHubError {
    fn from(error: HaiderError) -> Self {
        Self::Store(error)
    }
}

impl From<SessionHubError> for DaemonError {
    fn from(error: SessionHubError) -> Self {
        match error {
            SessionHubError::Store(error) => Self::Store(error),
            other => Self::Task {
                message: other.to_string(),
            },
        }
    }
}

// ──────────── hub: append seam, attachment lifecycle, shutdown ──────────────

impl SessionHub {
    /// Creates a hub with production's no-op boundary observer.
    pub fn new(
        store: SqliteStoreHandle,
        config: SessionHubConfig,
    ) -> Result<Self, SessionHubError> {
        Self::with_observer(store, config, Arc::new(NoopObserver))
    }

    /// Creates a hub with a semantic-boundary observer.
    pub fn with_observer(
        store: SqliteStoreHandle,
        config: SessionHubConfig,
        observer: Arc<dyn SessionHubObserver>,
    ) -> Result<Self, SessionHubError> {
        if config.actor_command_capacity == 0
            || config.catch_up_capacity == 0
            || config.catch_up_byte_budget == 0
            || config.replay_page_byte_budget == 0
        {
            return Err(SessionHubError::InvalidConfig(
                "session hub queue capacities and byte budgets must be greater than zero".into(),
            ));
        }
        if config.max_attachments_per_connection == 0 || config.max_attachments == 0 {
            return Err(SessionHubError::InvalidConfig(
                "session hub attachment limits must be greater than zero".into(),
            ));
        }
        let device_id = DeviceId::new(format!("daemon-session-hub-{}", store.worker_generation()));
        Ok(Self {
            inner: Arc::new(HubInner {
                store,
                config,
                observer,
                actors: Mutex::new(HashMap::new()),
                actor_tasks: Mutex::new(Vec::new()),
                replay_tasks: Mutex::new(Vec::new()),
                attachments: Mutex::new(HashMap::new()),
                attachment_slots: Mutex::new(AttachmentSlots::default()),
                draining: AtomicBool::new(false),
                device_id,
            }),
        })
    }

    /// Opens one logical connection after handshake negotiation.
    ///
    /// W3c/W3d seam: every real client — CLI attach, TUI live-attach, web —
    /// reaches sessions only through the returned [`HubConnection`];
    /// `connection.rs` is the first transport over it.
    pub fn open_connection(
        &self,
        capabilities: CapabilitySet,
        sink: Arc<dyn FrameSink>,
    ) -> Result<HubConnection, SessionHubError> {
        if self.inner.draining.load(Ordering::Acquire) {
            return Err(SessionHubError::Closed);
        }
        Ok(HubConnection {
            hub: self.clone(),
            connection_id: random_id("connection")?,
            capabilities,
            sink,
            closed: AtomicBool::new(false),
        })
    }

    /// Routes a worker append through the owning session actor.
    ///
    /// This is the only legal live-daemon append seam: INVARIANTs 1 and 2
    /// (module doc) are properties of the actor's command order, so an append
    /// that bypassed the actor could publish around a registration. That
    /// exclusivity holds by DISCIPLINE, not code shape —
    /// `SqliteStoreHandle::append` remains directly callable (tests seed with
    /// it; recovery runs before any hub exists). W3c must hand every live
    /// worker this hub as its [`StoreHandle`], as `register_harness` documents.
    pub async fn append(
        &self,
        envelopes: &mut [RawEnvelope],
    ) -> Result<haider_core::CommittedRange, HaiderError> {
        let Some(first) = envelopes.first() else {
            return Err(HaiderError::new(
                ErrorCode::InvalidArgument,
                "cannot append an empty envelope batch",
                false,
            ));
        };
        let actor = self
            .actor_for(first.session_id.clone())
            .await
            .map_err(hub_error_as_store)?;
        let (completed, result) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::Append {
                envelopes: envelopes.to_vec(),
                completed,
            })
            .await
            .map_err(|_| hub_closed_store_error())?;
        self.inner.observer.observe(HubObservation::AppendEnqueued {
            session_id: first.session_id.clone(),
        });
        let committed = result.await.map_err(|_| hub_closed_store_error())??;
        envelopes.clone_from_slice(&committed);
        let first_seq = committed.first().map_or(0, |envelope| envelope.seq);
        let last_seq = committed.last().map_or(0, |envelope| envelope.seq);
        Ok(haider_core::CommittedRange {
            first_seq,
            last_seq,
        })
    }

    /// Registers the live harness that consumes already-committed menu events.
    ///
    /// W3c auto-start seam: a worker spawned for a session registers here so
    /// committed menu resolutions wake it. Harness persistence must use this
    /// hub as its [`StoreHandle`], keeping every worker append inside the same
    /// session-actor order as attachment registration and publication.
    pub async fn register_harness(
        &self,
        session_id: SessionId,
        harness: HarnessHandle,
    ) -> Result<(), SessionHubError> {
        let actor = self.actor_for(session_id).await?;
        actor
            .commands
            .send(ActorCommand::RegisterHarness { harness })
            .await
            .map_err(|_| SessionHubError::Closed)
    }

    async fn actor_for(
        &self,
        session_id: SessionId,
    ) -> Result<SessionActorHandle, SessionHubError> {
        if self.inner.draining.load(Ordering::Acquire) {
            return Err(SessionHubError::Closed);
        }
        {
            let actors = lock(&self.inner.actors)?;
            if let Some(actor) = actors.get(&session_id) {
                return Ok(actor.clone());
            }
        }
        let head = self.inner.store.latest_seq(&session_id).await?;
        let last = if head == 0 {
            None
        } else {
            self.inner
                .store
                .read(&session_id, head.saturating_sub(1), 1)
                .await?
                .into_iter()
                .next()
        };
        let mut actors = lock(&self.inner.actors)?;
        if self.inner.draining.load(Ordering::Acquire) {
            return Err(SessionHubError::Closed);
        }
        if let Some(actor) = actors.get(&session_id) {
            return Ok(actor.clone());
        }
        let authority_epoch = last.as_ref().map_or(0, |envelope| envelope.authority_epoch);
        let (commands, receiver) = mpsc::channel(self.inner.config.actor_command_capacity);
        let stopping = Arc::new(AtomicBool::new(false));
        let actor = SessionActorHandle {
            commands,
            stopping: Arc::clone(&stopping),
        };
        let mut actor_tasks = lock(&self.inner.actor_tasks)?;
        let task = tokio::spawn(run_session_actor(
            session_id.clone(),
            head,
            authority_epoch,
            self.inner.store.worker_generation(),
            self.inner.config.catch_up_byte_budget,
            self.inner.store.clone(),
            Arc::clone(&self.inner.observer),
            stopping,
            receiver,
        ));
        actor_tasks.push(task);
        actors.insert(session_id, actor.clone());
        Ok(actor)
    }

    async fn register(
        &self,
        connection_id: &str,
        session_id: SessionId,
        after_seq: u64,
        mode: AttachMode,
    ) -> Result<RegisterResult, SessionHubError> {
        // Admission cap: the slot is reserved before any actor or channel
        // work and released on every non-registered outcome; a registered
        // attachment keeps its slot until `take_attachment` removes it.
        if let Some(message) = self.reserve_attachment_slot(connection_id)? {
            return Ok(RegisterResult::Overloaded { message });
        }
        let registered = self
            .register_reserved(connection_id, session_id, after_seq, mode)
            .await;
        if !matches!(registered, Ok(RegisterResult::Registered(_))) {
            self.release_attachment_slot(connection_id);
        }
        registered
    }

    /// Reserves one attachment admission slot, or reports why it cannot.
    fn reserve_attachment_slot(
        &self,
        connection_id: &str,
    ) -> Result<Option<String>, SessionHubError> {
        let config = &self.inner.config;
        let mut slots = lock(&self.inner.attachment_slots)?;
        let held = slots
            .per_connection
            .get(connection_id)
            .copied()
            .unwrap_or(0);
        if held >= config.max_attachments_per_connection {
            return Ok(Some(format!(
                "connection already holds its maximum of {} attachments; detach one and retry",
                config.max_attachments_per_connection
            )));
        }
        if slots.total >= config.max_attachments {
            return Ok(Some(format!(
                "daemon already holds its maximum of {} attachments; retry later",
                config.max_attachments
            )));
        }
        slots.total = slots.total.saturating_add(1);
        *slots
            .per_connection
            .entry(connection_id.to_owned())
            .or_insert(0) = held.saturating_add(1);
        Ok(None)
    }

    fn release_attachment_slot(&self, connection_id: &str) {
        let Ok(mut slots) = self.inner.attachment_slots.lock() else {
            return;
        };
        slots.total = slots.total.saturating_sub(1);
        if let Some(held) = slots.per_connection.get_mut(connection_id) {
            *held = held.saturating_sub(1);
            if *held == 0 {
                slots.per_connection.remove(connection_id);
            }
        }
    }

    async fn register_reserved(
        &self,
        connection_id: &str,
        session_id: SessionId,
        after_seq: u64,
        mode: AttachMode,
    ) -> Result<RegisterResult, SessionHubError> {
        let actor = self.actor_for(session_id.clone()).await?;
        let attachment_id = AttachmentId::new(random_id("attachment")?);
        let (events, event_receiver) = mpsc::channel(self.inner.config.catch_up_capacity);
        let (lagged, lag_receiver) = watch::channel(Option::<u64>::None);
        let catch_up_bytes = Arc::new(AtomicUsize::new(0));
        let (completed, result) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::Register {
                attachment_id: attachment_id.clone(),
                after_seq,
                events,
                lagged,
                queued_bytes: Arc::clone(&catch_up_bytes),
                completed,
            })
            .await
            .map_err(|_| SessionHubError::Closed)?;
        let attach_state = match result.await.map_err(|_| SessionHubError::Closed)? {
            ActorRegisterResult::Registered(attach_state) => attach_state,
            ActorRegisterResult::CursorAhead { requested, head } => {
                return Ok(RegisterResult::CursorAhead { requested, head });
            }
        };
        let registration = Registration {
            attachment_id: attachment_id.clone(),
            attach_state,
            actor: actor.clone(),
            events: event_receiver,
            lagged: lag_receiver,
            catch_up_bytes,
        };
        let (cancel, _) = watch::channel(false);
        lock(&self.inner.attachments)?.insert(
            attachment_id,
            AttachmentOwner {
                connection_id: connection_id.to_owned(),
                session_id,
                mode,
                actor,
                cancel,
            },
        );
        Ok(RegisterResult::Registered(registration))
    }

    fn spawn_replay(
        &self,
        registration: Registration,
        after_seq: u64,
        sink: Arc<dyn FrameSink>,
    ) -> Result<(), SessionHubError> {
        if self.inner.draining.load(Ordering::Acquire) {
            return Err(SessionHubError::Closed);
        }
        let cancel = lock(&self.inner.attachments)?
            .get(&registration.attachment_id)
            .map(|owner| owner.cancel.subscribe())
            .ok_or(SessionHubError::Closed)?;
        let mut replay_tasks = lock(&self.inner.replay_tasks)?;
        if self.inner.draining.load(Ordering::Acquire) {
            return Err(SessionHubError::Closed);
        }
        // Finished handles carry no live task ownership. Reap them on each
        // admission so repeated attach/detach cannot grow this registry.
        replay_tasks.retain(|handle| !handle.is_finished());
        let hub = self.clone();
        let task = tokio::spawn(async move {
            run_replay(hub, registration, after_seq, sink, cancel).await;
        });
        // The lock stays held from the drain recheck through spawn+push, so
        // shutdown's registry take either owns this task or rejects it before
        // it exists. No aborted-but-unjoined admission gap is possible.
        replay_tasks.push(task);
        Ok(())
    }

    fn take_attachment(
        &self,
        attachment_id: &AttachmentId,
        connection_id: Option<&str>,
    ) -> Result<Option<AttachmentOwner>, SessionHubError> {
        let mut attachments = lock(&self.inner.attachments)?;
        let owned = attachments
            .get(attachment_id)
            .is_some_and(|owner| connection_id.is_none_or(|id| owner.connection_id == id));
        if !owned {
            return Ok(None);
        }
        let owner = attachments.remove(attachment_id);
        if let Some(owner) = owner.as_ref() {
            let _ = owner.cancel.send(true);
            self.release_attachment_slot(&owner.connection_id);
        }
        Ok(owner)
    }

    async fn finish_detach(
        attachment_id: &AttachmentId,
        owner: AttachmentOwner,
    ) -> Result<(), SessionHubError> {
        owner
            .actor
            .commands
            .send(ActorCommand::Detach {
                attachment_id: attachment_id.clone(),
            })
            .await
            .map_err(|_| SessionHubError::Closed)
    }

    async fn detach(&self, attachment_id: &AttachmentId) -> Result<bool, SessionHubError> {
        let owner = self.take_attachment(attachment_id, None)?;
        let Some(owner) = owner else {
            return Ok(false);
        };
        Self::finish_detach(attachment_id, owner).await?;
        Ok(true)
    }

    async fn detach_connection(&self, connection_id: &str) -> Result<(), SessionHubError> {
        let attachments = {
            let owners = lock(&self.inner.attachments)?;
            owners
                .iter()
                .filter(|(_, owner)| owner.connection_id == connection_id)
                .map(|(attachment_id, _)| attachment_id.clone())
                .collect::<Vec<_>>()
        };
        for attachment_id in attachments {
            let _ = self.detach(&attachment_id).await?;
        }
        Ok(())
    }

    /// True when `connection_id` holds a CONTROL attachment to `session_id` —
    /// the menu-answer policy documented on [`HubConnection::menu_answer`].
    fn attachment_for_menu(
        &self,
        connection_id: &str,
        session_id: &SessionId,
    ) -> Result<bool, SessionHubError> {
        Ok(lock(&self.inner.attachments)?.values().any(|owner| {
            owner.connection_id == connection_id
                && owner.session_id == *session_id
                && matches!(owner.mode, AttachMode::Control)
        }))
    }

    fn try_send_attachment(
        &self,
        attachment_id: &AttachmentId,
        sink: &Arc<dyn FrameSink>,
        frame: WireFrame,
    ) -> Result<(), FrameSendError> {
        let attachments = lock(&self.inner.attachments).map_err(|_| FrameSendError)?;
        if !attachments.contains_key(attachment_id) {
            return Err(FrameSendError);
        }
        // The ownership lock makes send-vs-detach atomic: detach removes the
        // owner before purging its lane, so no frame can appear after purge.
        sink.try_send(frame)
    }

    /// Rejects new hub work synchronously before the runtime announces drain.
    pub fn begin_draining(&self) {
        self.inner.draining.store(true, Ordering::Release);
    }

    /// Begins drain, cancels and joins every replay, then stops and joins every
    /// session actor. No task retaining the store survives this method.
    pub async fn shutdown(&self) -> Result<(), SessionHubError> {
        self.begin_draining();
        // Install both abort-on-drop guards before the first await. If the
        // global drain deadline cancels this future, no hub task is detached.
        let replay_tasks = std::mem::take(&mut *lock(&self.inner.replay_tasks)?);
        let mut replay_tasks = OwnedTasks::new(replay_tasks);
        let actors = {
            let mut actors = lock(&self.inner.actors)?;
            actors.drain().map(|(_, actor)| actor).collect::<Vec<_>>()
        };
        let actor_tasks = std::mem::take(&mut *lock(&self.inner.actor_tasks)?);
        let mut actor_tasks = OwnedTasks::new(actor_tasks);
        for actor in &actors {
            actor.stopping.store(true, Ordering::Release);
        }
        self.inner.observer.observe(HubObservation::ShutdownGuarded);
        let owners = {
            let mut owners = lock(&self.inner.attachments)?;
            owners.drain().collect::<Vec<_>>()
        };
        // The drained owners bypass `take_attachment`; clear their admission
        // ledger wholesale (no new reservation is admitted while draining).
        *lock(&self.inner.attachment_slots)? = AttachmentSlots::default();
        for (_, owner) in &owners {
            let _ = owner.cancel.send(true);
        }
        replay_tasks.abort_all();
        replay_tasks.join_all().await;
        for actor in actors {
            let _ = actor.commands.send(ActorCommand::Stop).await;
        }
        actor_tasks.join_all().await;
        Ok(())
    }
}

/// The worker-facing store surface (see [`SessionHub::append`] for the law and
/// its discipline caveat). Reads go straight to the store: committed history
/// needs no actor serialization.
#[async_trait]
impl StoreHandle for SessionHub {
    async fn append(
        &self,
        envelopes: &mut [RawEnvelope],
    ) -> Result<haider_core::CommittedRange, HaiderError> {
        SessionHub::append(self, envelopes).await
    }

    async fn read(
        &self,
        session_id: &SessionId,
        since_seq: u64,
        limit: usize,
    ) -> Result<Vec<RawEnvelope>, HaiderError> {
        self.inner.store.read(session_id, since_seq, limit).await
    }

    async fn latest_seq(&self, session_id: &SessionId) -> Result<u64, HaiderError> {
        self.inner.store.latest_seq(session_id).await
    }
}

// ─────────── connection RPC surface: list/read/attach/detach/menu ───────────

impl HubConnection {
    /// Handles one request and enqueues its correlated response.
    pub async fn request(
        &self,
        request_id: RequestId,
        body: RequestBody,
    ) -> Result<(), SessionHubError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(SessionHubError::Closed);
        }
        if self.hub.inner.draining.load(Ordering::Acquire) {
            return self.respond_error(
                request_id,
                ERROR_CODE_DRAINING,
                "daemon is draining",
                true,
                None,
            );
        }
        match body {
            RequestBody::SessionList { cursor, limit } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.session_list(request_id, cursor, limit).await
            }
            RequestBody::SessionRead { session_id, range } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.session_read(request_id, session_id, range).await
            }
            RequestBody::SessionAttach {
                session_id,
                after_seq,
                mode,
            } => {
                let operation = match mode {
                    AttachMode::View => Operation::View,
                    AttachMode::Control => Operation::Control,
                    // `Unknown` and any future mode: never guess an
                    // authorization level for a mode this daemon predates.
                    _ => {
                        return self.respond_error(
                            request_id,
                            ERROR_CODE_INVALID_ARGUMENT,
                            "unknown attachment mode",
                            false,
                            None,
                        );
                    }
                };
                if let Err(message) = authorize(&self.capabilities, operation) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.session_attach(request_id, session_id, after_seq, mode)
                    .await
            }
            RequestBody::SessionDetach { attachment_id } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.session_detach(request_id, attachment_id).await
            }
            // `Unknown` and any future method decode alike: a typed,
            // correlated rejection instead of a dropped request.
            _ => self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "unknown session method",
                false,
                None,
            ),
        }
    }

    async fn session_list(
        &self,
        request_id: RequestId,
        cursor: Option<String>,
        limit: u32,
    ) -> Result<(), SessionHubError> {
        let after = match cursor.as_deref().map(decode_cursor).transpose() {
            Ok(after) => after,
            Err(()) => {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_CURSOR,
                    "session-list cursor is invalid",
                    false,
                    None,
                );
            }
        };
        let limit = usize::try_from(limit)
            .unwrap_or(usize::MAX)
            .min(MAX_LIST_PAGE);
        if limit == 0 {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "session-list limit must be greater than zero",
                false,
                None,
            );
        }
        let ids = self.hub.inner.store.session_ids().await?;
        let mut selected = ids
            .into_iter()
            .filter(|session_id| {
                after
                    .as_ref()
                    .is_none_or(|after| session_id.as_str() > after.as_str())
            })
            .take(limit.saturating_add(1))
            .collect::<Vec<_>>();
        let has_more = selected.len() > limit;
        if has_more {
            selected.truncate(limit);
        }
        let mut sessions = Vec::with_capacity(selected.len());
        for session_id in &selected {
            sessions.push(SessionSummary {
                session_id: session_id.clone(),
                head_seq: self.hub.inner.store.latest_seq(session_id).await?,
                worker_generation: self.hub.inner.store.worker_generation(),
            });
        }
        let next_cursor = has_more
            .then(|| selected.last().map(encode_cursor))
            .flatten();
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionList {
                sessions,
                next_cursor,
            },
        })
    }

    async fn session_read(
        &self,
        request_id: RequestId,
        session_id: SessionId,
        range: SeqRange,
    ) -> Result<(), SessionHubError> {
        let head = self.hub.inner.store.latest_seq(&session_id).await?;
        if head == 0 {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "session was not found",
                false,
                None,
            );
        }
        if range.start_seq == 0 || range.end_seq < range.start_seq {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "session-read range must be non-empty and start at sequence one or later",
                false,
                None,
            );
        }
        let count = range
            .end_seq
            .saturating_sub(range.start_seq)
            .saturating_add(1);
        let limit = usize::try_from(count).unwrap_or(usize::MAX);
        if limit > MAX_READ_ENVELOPES {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "session-read range exceeds the maximum of 1024 envelopes",
                false,
                None,
            );
        }
        let envelopes = self
            .hub
            .inner
            .store
            .read(&session_id, range.start_seq.saturating_sub(1), limit)
            .await?
            .into_iter()
            .take_while(|envelope| envelope.seq <= range.end_seq)
            .collect::<Vec<_>>();
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionRead {
                result: SessionReadResult {
                    session_id,
                    range,
                    head_seq: head,
                    envelopes,
                },
            },
        })
    }

    async fn session_attach(
        &self,
        request_id: RequestId,
        session_id: SessionId,
        after_seq: u64,
        mode: AttachMode,
    ) -> Result<(), SessionHubError> {
        if self.hub.inner.store.latest_seq(&session_id).await? == 0 {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "session was not found",
                false,
                None,
            );
        }
        let registration = match self
            .hub
            .register(&self.connection_id, session_id, after_seq, mode)
            .await?
        {
            RegisterResult::Registered(registration) => registration,
            RegisterResult::CursorAhead { requested, head } => {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_CURSOR_AHEAD,
                    "replay cursor is beyond the committed session head",
                    false,
                    Some(ErrorData::CursorAhead { requested, head }),
                );
            }
            // Same stable code the connection cap uses (its doc names
            // admission caps as the family); correlated and retryable here.
            RegisterResult::Overloaded { message } => {
                return self.respond_error(request_id, ERROR_CODE_OVERLOADED, &message, true, None);
            }
        };
        let attachment_id = registration.attachment_id.clone();
        let attach_state = registration.attach_state.clone();
        if let Err(error) = self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionAttach {
                attachment_id: attachment_id.clone(),
                attach_state,
            },
        }) {
            let _ = self.hub.detach(&attachment_id).await;
            return Err(error);
        }
        self.hub
            .spawn_replay(registration, after_seq, Arc::clone(&self.sink))
    }

    async fn session_detach(
        &self,
        request_id: RequestId,
        attachment_id: AttachmentId,
    ) -> Result<(), SessionHubError> {
        let owner = self
            .hub
            .take_attachment(&attachment_id, Some(&self.connection_id))?;
        let Some(owner) = owner else {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "attachment was not found on this connection",
                false,
                None,
            );
        };
        // Removal/cancellation happened under the same ownership lock used by
        // replay delivery. Purging now is therefore a terminal lane barrier.
        self.sink.purge_attachment(&attachment_id);
        SessionHub::finish_detach(&attachment_id, owner).await?;
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionDetach { attachment_id },
        })
    }

    /// Handles the durable top-level `MenuAnswer` command.
    ///
    /// The arbitration law — first COMMITTED answer wins, losers get the
    /// winner's `resolution_seq` — is stated on
    /// `haider_store::Store::resolve_menu`; this method adds transport
    /// concerns only: capability + attachment policy, wire error mapping, and
    /// the correlated reply. Every attachment learns the outcome from the
    /// event stream (the actor publishes the committed envelope); the reply
    /// is a convenience, never the authority.
    ///
    /// Policy decision (brief §6): answering requires a CONTROL attachment to
    /// the target session — v0.1 has no "controller without a viewport"
    /// allowance.
    #[allow(clippy::too_many_arguments)]
    pub async fn menu_answer(
        &self,
        request_id: Option<RequestId>,
        command_id: CommandId,
        session_id: SessionId,
        menu_id: haider_protocol::ids::MenuId,
        request_seq: u64,
        worker_generation: u64,
        option_key: String,
        option_index: u32,
        input: Option<MenuInput>,
    ) -> Result<(), SessionHubError> {
        if self.hub.inner.draining.load(Ordering::Acquire) {
            return self.menu_error(
                request_id,
                ERROR_CODE_DRAINING,
                "daemon is draining",
                true,
                None,
            );
        }
        if let Err(message) = authorize(&self.capabilities, Operation::Control) {
            return self.menu_error(
                request_id,
                ERROR_CODE_CAPABILITY_DENIED,
                message,
                false,
                None,
            );
        }
        if !self
            .hub
            .attachment_for_menu(&self.connection_id, &session_id)?
        {
            return self.menu_error(
                request_id,
                ERROR_CODE_CAPABILITY_DENIED,
                "menu answers require a control attachment to this session",
                false,
                None,
            );
        }
        let (value, secret_reference) = match input {
            Some(MenuInput::Text { text }) => (Some(text), false),
            Some(MenuInput::SecretVaultReference { vault_reference }) => {
                (Some(vault_reference), true)
            }
            None => (None, false),
            Some(_) => {
                return self.menu_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    "unknown menu input kind",
                    false,
                    None,
                );
            }
        };
        let answer = DurableMenuAnswer {
            menu: menu_id,
            option_key: (!option_key.is_empty()).then_some(option_key),
            option_index,
            value,
            via: AnswerVia::Rpc,
        };
        let actor = self.hub.actor_for(session_id.clone()).await?;
        let command = MenuResolutionCommand {
            command_id: command_id.0,
            session_id,
            request_seq,
            worker_generation,
            answer,
            device_id: self.hub.inner.device_id.clone(),
            input_is_secret_reference: secret_reference,
        };
        let (completed, result) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::MenuAnswer { command, completed })
            .await
            .map_err(|_| SessionHubError::Closed)?;
        match result.await.map_err(|_| SessionHubError::Closed)? {
            Ok(MenuResolutionOutcome::Committed { ref envelope }) => {
                self.menu_success(request_id, envelope.seq)
            }
            Ok(MenuResolutionOutcome::IdempotentReplay { resolution_seq }) => {
                self.menu_success(request_id, resolution_seq)
            }
            Ok(MenuResolutionOutcome::AlreadyResolved { resolution_seq }) => self.menu_error(
                request_id,
                ERROR_CODE_ALREADY_RESOLVED,
                "menu was already resolved",
                false,
                Some(ErrorData::AlreadyResolved { resolution_seq }),
            ),
            Err(error) => {
                let code = match error.code {
                    ErrorCode::SingleWriterViolation => ERROR_CODE_STALE_GENERATION,
                    ErrorCode::MenuAlreadyAnswered => ERROR_CODE_ALREADY_RESOLVED,
                    ErrorCode::MenuNotFound | ErrorCode::SessionNotFound => ERROR_CODE_NOT_FOUND,
                    _ => ERROR_CODE_INVALID_ARGUMENT,
                };
                self.menu_error(request_id, code, &error.message, error.retryable, None)
            }
        }
    }

    fn menu_success(
        &self,
        request_id: Option<RequestId>,
        resolution_seq: u64,
    ) -> Result<(), SessionHubError> {
        match request_id {
            Some(request_id) => self.send(WireFrame::Response {
                request_id,
                body: ResponseBody::MenuAnswer { resolution_seq },
            }),
            None => Ok(()),
        }
    }

    fn menu_error(
        &self,
        request_id: Option<RequestId>,
        code: &str,
        message: &str,
        retryable: bool,
        data: Option<ErrorData>,
    ) -> Result<(), SessionHubError> {
        match request_id {
            Some(request_id) => self.respond_error(request_id, code, message, retryable, data),
            None => self.send(WireFrame::ProtocolError(ProtocolError {
                code: code.into(),
                message: message.into(),
                fatal: false,
            })),
        }
    }

    fn respond_error(
        &self,
        request_id: RequestId,
        code: &str,
        message: &str,
        retryable: bool,
        data: Option<ErrorData>,
    ) -> Result<(), SessionHubError> {
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::Error {
                code: code.into(),
                message: message.into(),
                retryable,
                data,
            },
        })
    }

    fn send(&self, frame: WireFrame) -> Result<(), SessionHubError> {
        self.sink
            .try_send(frame)
            .map_err(|_| SessionHubError::Delivery)
    }

    /// Detaches every attachment owned by this connection.
    pub async fn close(&self) -> Result<(), SessionHubError> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.hub.detach_connection(&self.connection_id).await
    }
}

#[derive(Debug, Clone, Copy)]
enum Operation {
    View,
    Control,
}

fn authorize(capabilities: &CapabilitySet, operation: Operation) -> Result<(), &'static str> {
    let allowed = match operation {
        Operation::View => {
            capabilities.contains(&Capability::View) || capabilities.contains(&Capability::Control)
        }
        Operation::Control => capabilities.contains(&Capability::Control),
    };
    allowed.then_some(()).ok_or(match operation {
        Operation::View => "this method requires the view capability",
        Operation::Control => "this method requires the control capability",
    })
}

// ──────────── session actor: the serialized command loop (§5.5) ─────────────

/// One session's entire command order, in one loop, in one task.
///
/// Both §5.5 invariants (module doc) hold by code shape here: the only awaits
/// inside any arm are the store calls (`append`, `resolve_menu`), publication
/// is a synchronous call after they return in the same arm, and the
/// `Register` arm contains no await at all. Adding an await between a store
/// return and its `publish`, or anywhere in `Register`, breaks a law — the
/// forced-boundary tests in `tests/session_hub_tests.rs` will catch it.
#[allow(clippy::too_many_arguments)]
async fn run_session_actor(
    session_id: SessionId,
    mut head: u64,
    mut authority_epoch: u64,
    worker_generation: u64,
    catch_up_byte_budget: usize,
    store: SqliteStoreHandle,
    observer: Arc<dyn SessionHubObserver>,
    stopping: Arc<AtomicBool>,
    mut commands: mpsc::Receiver<ActorCommand>,
) {
    let mut attachments = HashMap::<AttachmentId, ActorAttachment>::new();
    let mut harness = Option::<HarnessHandle>::None;
    while let Some(command) = commands.recv().await {
        if stopping.load(Ordering::Acquire) {
            break;
        }
        match command {
            ActorCommand::Append {
                mut envelopes,
                completed,
            } => {
                // INVARIANT 1 (module doc): the append is awaited here, and
                // `publish` below is synchronous in this same turn.
                let result = store.append(&mut envelopes).await;
                match result {
                    Ok(range) => {
                        head = range.last_seq;
                        if let Some(last) = envelopes.last() {
                            authority_epoch = last.authority_epoch;
                        }
                        observer.observe(HubObservation::Persisted {
                            session_id: session_id.clone(),
                            through_seq: head,
                        });
                        publish(&mut attachments, &envelopes, catch_up_byte_budget);
                        observer.observe(HubObservation::Published {
                            session_id: session_id.clone(),
                            through_seq: head,
                        });
                        let _ = completed.send(Ok(envelopes));
                    }
                    Err(error) => {
                        let _ = completed.send(Err(error));
                    }
                }
            }
            ActorCommand::Register {
                attachment_id,
                after_seq,
                events,
                lagged,
                queued_bytes,
                completed,
            } => {
                if after_seq > head {
                    let _ = completed.send(ActorRegisterResult::CursorAhead {
                        requested: after_seq,
                        head,
                    });
                    continue;
                }
                attachments.insert(
                    attachment_id.clone(),
                    ActorAttachment {
                        events,
                        lagged,
                        queued_bytes,
                        last_buffered_seq: head,
                        active: true,
                    },
                );
                observer.observe(HubObservation::ReceiverRegistered {
                    attachment_id: attachment_id.clone(),
                });
                // INVARIANT 2 (module doc): receiver insertion above and this
                // head read are adjacent synchronous statements in one actor
                // turn — no await or yield between them.
                let high_water = head;
                observer.observe(HubObservation::HeadCaptured {
                    attachment_id: attachment_id.clone(),
                    head: high_water,
                });
                let _ = completed.send(ActorRegisterResult::Registered(AttachState {
                    session_id: session_id.clone(),
                    requested_after_seq: after_seq,
                    replay_through_seq: high_water,
                    worker_generation,
                    authority_epoch,
                }));
            }
            ActorCommand::Reregister {
                attachment_id,
                events,
                lagged,
                completed,
            } => {
                let registered = attachments.get_mut(&attachment_id).map(|attachment| {
                    attachment.events = events;
                    attachment.lagged = lagged;
                    // The old channel and everything queued on it are dropped
                    // wholesale, and the replay task credits nothing after it
                    // requests re-registration, so zero is the exact balance.
                    attachment.queued_bytes.store(0, Ordering::Release);
                    attachment.last_buffered_seq = head;
                    attachment.active = true;
                    head
                });
                let _ = completed.send(registered);
            }
            ActorCommand::Detach { attachment_id } => {
                attachments.remove(&attachment_id);
            }
            ActorCommand::MenuAnswer { command, completed } => {
                // The CAS itself (first-committed-wins) is
                // `Store::resolve_menu`'s law; this arm only serializes it
                // with appends and publishes a committed envelope afterwards
                // (INVARIANT 1 shape).
                let outcome = store.resolve_menu(command).await;
                if let Ok(MenuResolutionOutcome::Committed { ref envelope }) = outcome {
                    head = envelope.seq;
                    authority_epoch = envelope.authority_epoch;
                    observer.observe(HubObservation::Persisted {
                        session_id: session_id.clone(),
                        through_seq: head,
                    });
                    publish(
                        &mut attachments,
                        std::slice::from_ref(envelope.as_ref()),
                        catch_up_byte_budget,
                    );
                    observer.observe(HubObservation::Published {
                        session_id: session_id.clone(),
                        through_seq: head,
                    });
                    if let Some(harness) = harness.as_ref()
                        && let Err(error) =
                            harness.apply_committed_menu_event(envelope.as_ref().clone())
                    {
                        tracing::warn!(
                            session_id = %session_id,
                            error = ?error,
                            "committed menu event could not wake the live harness"
                        );
                    }
                }
                let _ = completed.send(outcome);
            }
            ActorCommand::RegisterHarness {
                harness: registered,
            } => harness = Some(registered),
            ActorCommand::Stop => break,
        }
    }
}

/// Fans committed envelopes out to every active attachment receiver.
///
/// Called only from [`run_session_actor`], synchronously, after the store
/// call returned (INVARIANT 1, module doc). `try_send` never blocks the
/// actor; a full receiver — full in FRAMES or in estimated BYTES — flips the
/// attachment inactive and reports its last buffered sequence on the lag
/// channel, and the replay task then resumes from the store
/// (store-is-the-lag-buffer, module doc). Bytes are charged before enqueue
/// and credited by the replay task on receive; an envelope is always admitted
/// into an EMPTY channel so one oversized envelope cannot wedge the
/// attachment in a lag loop.
fn publish(
    attachments: &mut HashMap<AttachmentId, ActorAttachment>,
    envelopes: &[RawEnvelope],
    byte_budget: usize,
) {
    // Weighed once per envelope, not once per attachment.
    let weights = envelopes
        .iter()
        .map(envelope_weight_bytes)
        .collect::<Vec<_>>();
    for attachment in attachments.values_mut() {
        if !attachment.active {
            continue;
        }
        for (envelope, weight) in envelopes.iter().zip(&weights) {
            let queued = attachment.queued_bytes.load(Ordering::Acquire);
            if queued > 0 && queued.saturating_add(*weight) > byte_budget {
                let _ = attachment.lagged.send(Some(attachment.last_buffered_seq));
                attachment.active = false;
                break;
            }
            attachment.queued_bytes.fetch_add(*weight, Ordering::AcqRel);
            match attachment.events.try_send(QueuedEnvelope {
                weight: *weight,
                envelope: envelope.clone(),
            }) {
                Ok(()) => attachment.last_buffered_seq = envelope.seq,
                Err(_) => {
                    attachment.queued_bytes.fetch_sub(*weight, Ordering::AcqRel);
                    let _ = attachment.lagged.send(Some(attachment.last_buffered_seq));
                    attachment.active = false;
                    break;
                }
            }
        }
    }
}

/// Deterministic, allocation-free estimate of one committed envelope's
/// serialized size, used only for catch-up budget accounting (never for wire
/// framing — the negotiated frame limit governs that at encode time).
fn envelope_weight_bytes(envelope: &RawEnvelope) -> usize {
    const ENVELOPE_FIELD_OVERHEAD: usize = 256;
    ENVELOPE_FIELD_OVERHEAD
        .saturating_add(envelope.event_id.as_str().len())
        .saturating_add(envelope.session_id.as_str().len())
        .saturating_add(json_weight_bytes(&envelope.payload))
}

fn json_weight_bytes(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Null | serde_json::Value::Bool(_) => 5,
        serde_json::Value::Number(_) => 20,
        serde_json::Value::String(text) => text.len().saturating_add(2),
        serde_json::Value::Array(items) => items
            .iter()
            .map(json_weight_bytes)
            .fold(2_usize, |total, weight| {
                total.saturating_add(weight).saturating_add(1)
            }),
        serde_json::Value::Object(fields) => fields
            .iter()
            .map(|(key, value)| {
                key.len()
                    .saturating_add(4)
                    .saturating_add(json_weight_bytes(value))
            })
            .fold(2_usize, |total, weight| total.saturating_add(weight)),
    }
}

// ──────── replay pipeline: replay → caught-up → buffered drain → live ───────

/// One attachment's delivery task, §5.5 steps 4-7.
///
/// Each outer iteration replays `(last_sent_seq, H]` from store pages, then
/// announces `AttachCaughtUp(H)`, drains the already-registered bounded
/// receiver for `seq > H` (duplicates dropped by seq), and goes live. A
/// lagged or overflowed receiver re-registers in actor order and re-enters
/// the outer loop with the new head — the store, not memory, carries what was
/// missed. Exit discipline: `break` still owns the attachment registration
/// and releases it at the bottom; every `return` path has already released
/// ownership (via `lag_and_detach`/`take_attachment`) or observed its
/// cancellation, which only fires after ownership was removed.
async fn run_replay(
    hub: SessionHub,
    mut registration: Registration,
    mut last_sent_seq: u64,
    sink: Arc<dyn FrameSink>,
    mut cancel: watch::Receiver<bool>,
) {
    let attachment_id = registration.attachment_id.clone();
    let session_id = registration.attach_state.session_id.clone();
    let mut high_water = registration.attach_state.replay_through_seq;
    loop {
        // Phase: store replay of (last_sent_seq, high_water].
        let replayed = replay_range(
            &hub,
            &sink,
            &attachment_id,
            &session_id,
            &mut last_sent_seq,
            high_water,
            &mut registration.lagged,
            &mut cancel,
        )
        .await;
        match replayed {
            ReplayStep::Continue => {}
            ReplayStep::ReceiverLagged => {
                match reregister(&hub, &registration.actor, &attachment_id).await {
                    Some((events, lagged, next_head)) => {
                        registration.events = events;
                        registration.lagged = lagged;
                        high_water = next_head;
                        continue;
                    }
                    None => break,
                }
            }
            ReplayStep::Cancelled => break,
            ReplayStep::OutboxFull => {
                lag_and_detach(&hub, &sink, &attachment_id, last_sent_seq).await;
                return;
            }
        }

        // Phase: (after_seq, H] is fully on the wire — announce H.
        match acquire_send_capacity(&sink, &attachment_id, &mut registration.lagged, &mut cancel)
            .await
        {
            CapacityWait::Ready(_) => {}
            CapacityWait::Cancelled => return,
            CapacityWait::LaggedWhileBlocked => {
                lag_and_detach(&hub, &sink, &attachment_id, last_sent_seq).await;
                return;
            }
        }
        hub.inner.observer.observe(HubObservation::BeforeCaughtUp {
            attachment_id: attachment_id.clone(),
            through_seq: high_water,
        });
        if hub
            .try_send_attachment(
                &attachment_id,
                &sink,
                WireFrame::AttachCaughtUp {
                    attachment_id: attachment_id.clone(),
                    high_water_seq: high_water,
                },
            )
            .is_err()
        {
            lag_and_detach(&hub, &sink, &attachment_id, last_sent_seq).await;
            return;
        }
        hub.inner.observer.observe(HubObservation::CaughtUp {
            attachment_id: attachment_id.clone(),
            through_seq: high_water,
        });

        // Phase: buffered drain — deliver `seq > H` already committed during
        // replay, dropping duplicates by seq (at-least-once, R11).
        let mut burst = 0_usize;
        loop {
            if *cancel.borrow() {
                return;
            }
            match registration.events.try_recv() {
                Ok(queued) => {
                    credit_catch_up(&registration.catch_up_bytes, queued.weight);
                    let envelope = queued.envelope;
                    if envelope.seq <= last_sent_seq || envelope.seq <= high_water {
                        continue;
                    }
                    if burst == 0 {
                        burst = match acquire_send_capacity(
                            &sink,
                            &attachment_id,
                            &mut registration.lagged,
                            &mut cancel,
                        )
                        .await
                        {
                            CapacityWait::Ready(capacity) => capacity,
                            CapacityWait::Cancelled => return,
                            CapacityWait::LaggedWhileBlocked => {
                                lag_and_detach(&hub, &sink, &attachment_id, last_sent_seq).await;
                                return;
                            }
                        };
                    }
                    burst -= 1;
                    if deliver_event(
                        &hub,
                        &sink,
                        &attachment_id,
                        &session_id,
                        envelope,
                        &mut last_sent_seq,
                        DeliveryPhase::Buffered,
                    )
                    .is_err()
                    {
                        lag_and_detach(&hub, &sink, &attachment_id, last_sent_seq).await;
                        return;
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    if registration.lagged.borrow().is_some() {
                        match reregister(&hub, &registration.actor, &attachment_id).await {
                            Some((events, lagged, next_head)) => {
                                registration.events = events;
                                registration.lagged = lagged;
                                high_water = next_head;
                                break;
                            }
                            None => return,
                        }
                    }
                    return;
                }
            }
        }
        if high_water > last_sent_seq {
            // A mid-drain re-registration raised the head: replay the gap
            // from the store before going live.
            continue;
        }

        // Phase: live — wait on cancellation, lag, or the next commit.
        loop {
            tokio::select! {
                biased;
                changed = cancel.changed() => {
                    if changed.is_err() || *cancel.borrow() {
                        return;
                    }
                }
                changed = registration.lagged.changed() => {
                    if changed.is_err() || registration.lagged.borrow().is_some() {
                        match reregister(&hub, &registration.actor, &attachment_id).await {
                            Some((events, lagged, next_head)) => {
                                registration.events = events;
                                registration.lagged = lagged;
                                high_water = next_head;
                                break;
                            }
                            None => return,
                        }
                    }
                }
                queued = registration.events.recv() => {
                    let Some(queued) = queued else {
                        return;
                    };
                    credit_catch_up(&registration.catch_up_bytes, queued.weight);
                    let envelope = queued.envelope;
                    if envelope.seq <= last_sent_seq {
                        continue;
                    }
                    match acquire_send_capacity(
                        &sink,
                        &attachment_id,
                        &mut registration.lagged,
                        &mut cancel,
                    )
                    .await
                    {
                        CapacityWait::Ready(_) => {}
                        CapacityWait::Cancelled => return,
                        CapacityWait::LaggedWhileBlocked => {
                            lag_and_detach(&hub, &sink, &attachment_id, last_sent_seq).await;
                            return;
                        }
                    }
                    if deliver_event(
                        &hub,
                        &sink,
                        &attachment_id,
                        &session_id,
                        envelope,
                        &mut last_sent_seq,
                        DeliveryPhase::Live,
                    ).is_err() {
                        lag_and_detach(&hub, &sink, &attachment_id, last_sent_seq).await;
                        return;
                    }
                }
            }
        }
    }
    // `break` exit: store failure, truncated page, or a closed actor. The
    // registration is still owned here; release it (a no-op if a concurrent
    // detach already took it).
    let _ = hub.detach(&attachment_id).await;
}

enum ReplayStep {
    Continue,
    ReceiverLagged,
    Cancelled,
    OutboxFull,
}

enum CapacityWait {
    /// The sink can admit this many frames right now.
    Ready(usize),
    Cancelled,
    /// Lag pressure arrived while the sink admitted nothing: commits are
    /// overflowing the catch-up buffer behind a stalled outbox.
    LaggedWhileBlocked,
}

/// Waits until the sink can admit at least one frame for this attachment.
///
/// Pacing law (NOW-3): delivery bursts never exceed the sink's REAL available
/// quota, and between bursts the task awaits the sink's drain-progress signal
/// — actual writer state, never a sleep — so a reading client cannot be
/// lagged just because a replay page outran its writer's schedule. Detachment
/// still happens immediately on an ACTUAL refusal (`try_send` failing, e.g.
/// the byte budget), and on [`CapacityWait::LaggedWhileBlocked`] — the
/// genuinely stuck shape. Sinks without a progress signal (tests) keep the
/// pre-pacing behavior: `try_send` is the only arbiter.
async fn acquire_send_capacity(
    sink: &Arc<dyn FrameSink>,
    attachment_id: &AttachmentId,
    lagged: &mut watch::Receiver<Option<u64>>,
    cancel: &mut watch::Receiver<bool>,
) -> CapacityWait {
    loop {
        if *cancel.borrow() {
            return CapacityWait::Cancelled;
        }
        // Subscribe BEFORE reading capacity so a frame popped between the
        // read and the await still marks this receiver changed (no lost
        // wakeup).
        let progress = sink.drain_progress();
        let capacity = sink.capacity_for(attachment_id);
        if capacity > 0 {
            return CapacityWait::Ready(capacity);
        }
        let Some(mut progress) = progress else {
            return CapacityWait::Ready(1);
        };
        tokio::select! {
            biased;
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    return CapacityWait::Cancelled;
                }
            }
            changed = lagged.changed() => {
                if changed.is_err() || lagged.borrow().is_some() {
                    return CapacityWait::LaggedWhileBlocked;
                }
            }
            changed = progress.changed() => {
                if changed.is_err() {
                    // The sink dropped its signal; arbitrate via try_send.
                    return CapacityWait::Ready(1);
                }
            }
        }
    }
}

/// Credits bytes back to the shared catch-up ledger as envelopes leave the
/// channel. Saturating: an (impossible by charge/credit symmetry) underflow
/// must degrade to zero, never wrap into a permanent phantom backlog.
fn credit_catch_up(catch_up_bytes: &Arc<AtomicUsize>, weight: usize) {
    let _ = catch_up_bytes.fetch_update(Ordering::AcqRel, Ordering::Acquire, |bytes| {
        Some(bytes.saturating_sub(weight))
    });
}

#[allow(clippy::too_many_arguments)]
async fn replay_range(
    hub: &SessionHub,
    sink: &Arc<dyn FrameSink>,
    attachment_id: &AttachmentId,
    session_id: &SessionId,
    last_sent_seq: &mut u64,
    high_water: u64,
    lagged: &mut watch::Receiver<Option<u64>>,
    cancel: &mut watch::Receiver<bool>,
) -> ReplayStep {
    while *last_sent_seq < high_water {
        // Byte-budgeted page (NOW-2): bounds the transient envelopes one page
        // may materialize; a short page just resumes from `last_sent_seq`.
        let read = hub.inner.store.read_page(
            session_id,
            *last_sent_seq,
            REPLAY_PAGE_SIZE,
            hub.inner.config.replay_page_byte_budget,
        );
        let page = tokio::select! {
            biased;
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    return ReplayStep::Cancelled;
                }
                continue;
            }
            changed = lagged.changed() => {
                if changed.is_err() || lagged.borrow().is_some() {
                    return ReplayStep::ReceiverLagged;
                }
                continue;
            }
            result = read => match result {
                Ok(page) => page,
                Err(error) => {
                    tracing::warn!(
                        session_id = %session_id,
                        attachment_id = %attachment_id,
                        error = ?error,
                        "attachment replay store read failed"
                    );
                    return ReplayStep::Cancelled;
                }
            }
        };
        if page.is_empty() {
            return ReplayStep::Cancelled;
        }
        // Pacing (NOW-3): the burst never exceeds what the sink can really
        // admit, so a page can no longer lag a client whose writer simply
        // had not run yet. An actual `try_send` refusal still detaches.
        let mut burst = 0_usize;
        for envelope in page
            .into_iter()
            .take_while(|envelope| envelope.seq <= high_water)
        {
            if *cancel.borrow() {
                return ReplayStep::Cancelled;
            }
            if envelope.seq <= *last_sent_seq {
                continue;
            }
            if burst == 0 {
                burst = match acquire_send_capacity(sink, attachment_id, lagged, cancel).await {
                    CapacityWait::Ready(capacity) => capacity,
                    CapacityWait::Cancelled => return ReplayStep::Cancelled,
                    CapacityWait::LaggedWhileBlocked => return ReplayStep::OutboxFull,
                };
            }
            burst -= 1;
            if deliver_event(
                hub,
                sink,
                attachment_id,
                session_id,
                envelope,
                last_sent_seq,
                DeliveryPhase::Replay,
            )
            .is_err()
            {
                return ReplayStep::OutboxFull;
            }
        }
    }
    ReplayStep::Continue
}

fn deliver_event(
    hub: &SessionHub,
    sink: &Arc<dyn FrameSink>,
    attachment_id: &AttachmentId,
    session_id: &SessionId,
    envelope: RawEnvelope,
    last_sent_seq: &mut u64,
    phase: DeliveryPhase,
) -> Result<(), FrameSendError> {
    let seq = envelope.seq;
    hub.inner.observer.observe(HubObservation::BeforeEvent {
        attachment_id: attachment_id.clone(),
        seq,
    });
    hub.try_send_attachment(
        attachment_id,
        sink,
        WireFrame::Event {
            attachment_id: attachment_id.clone(),
            session_id: session_id.clone(),
            envelope,
        },
    )?;
    *last_sent_seq = seq;
    hub.inner.observer.observe(match phase {
        DeliveryPhase::Buffered => HubObservation::BufferedEvent {
            attachment_id: attachment_id.clone(),
            seq,
        },
        DeliveryPhase::Replay => HubObservation::ReplayEvent {
            attachment_id: attachment_id.clone(),
            seq,
        },
        DeliveryPhase::Live => HubObservation::LiveEvent {
            attachment_id: attachment_id.clone(),
            seq,
        },
    });
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum DeliveryPhase {
    Replay,
    Buffered,
    Live,
}

/// Replaces the catch-up channels in actor order after lag/overflow. The
/// actor resets the shared byte ledger to zero in the same turn, matching the
/// wholesale drop of the old channel's contents.
async fn reregister(
    hub: &SessionHub,
    actor: &SessionActorHandle,
    attachment_id: &AttachmentId,
) -> Option<(
    mpsc::Receiver<QueuedEnvelope>,
    watch::Receiver<Option<u64>>,
    u64,
)> {
    let (events, event_receiver) = mpsc::channel(hub.inner.config.catch_up_capacity);
    let (lagged, lag_receiver) = watch::channel(None);
    let (completed, result) = oneshot::channel();
    actor
        .commands
        .send(ActorCommand::Reregister {
            attachment_id: attachment_id.clone(),
            events,
            lagged,
            completed,
        })
        .await
        .ok()?;
    let head = result.await.ok().flatten()?;
    Some((event_receiver, lag_receiver, head))
}

async fn lag_and_detach(
    hub: &SessionHub,
    sink: &Arc<dyn FrameSink>,
    attachment_id: &AttachmentId,
    last_queued_seq: u64,
) {
    let Ok(Some(owner)) = hub.take_attachment(attachment_id, None) else {
        return;
    };
    sink.purge_attachment(attachment_id);
    let _ = sink.try_send(WireFrame::Lagged {
        attachment_id: attachment_id.clone(),
        last_queued_seq,
    });
    let _ = SessionHub::finish_detach(attachment_id, owner).await;
}

// ────────────────────────────── small helpers ───────────────────────────────

fn encode_cursor(session_id: &SessionId) -> String {
    let mut cursor = String::from("hs1.");
    for byte in session_id.as_str().as_bytes() {
        use std::fmt::Write as _;
        let _ = write!(&mut cursor, "{byte:02x}");
    }
    cursor
}

fn decode_cursor(cursor: &str) -> Result<SessionId, ()> {
    let encoded = cursor.strip_prefix("hs1.").ok_or(())?;
    if encoded.len() % 2 != 0 {
        return Err(());
    }
    let mut bytes = Vec::with_capacity(encoded.len() / 2);
    for pair in encoded.as_bytes().chunks_exact(2) {
        let pair = std::str::from_utf8(pair).map_err(|_| ())?;
        bytes.push(u8::from_str_radix(pair, 16).map_err(|_| ())?);
    }
    String::from_utf8(bytes).map(SessionId::new).map_err(|_| ())
}

fn random_id(prefix: &str) -> Result<String, SessionHubError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| {
        SessionHubError::Task(format!("cannot generate {prefix} identity: {error}"))
    })?;
    let mut id = String::with_capacity(prefix.len().saturating_add(33));
    id.push_str(prefix);
    id.push('-');
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut id, "{byte:02x}");
    }
    Ok(id)
}

fn lock<T>(mutex: &Mutex<T>) -> Result<std::sync::MutexGuard<'_, T>, SessionHubError> {
    mutex
        .lock()
        .map_err(|_| SessionHubError::Task("session hub mutex is poisoned".into()))
}

fn hub_error_as_store(error: SessionHubError) -> HaiderError {
    match error {
        SessionHubError::Store(error) => error,
        other => HaiderError::new(ErrorCode::Internal, other.to_string(), false),
    }
}

fn hub_closed_store_error() -> HaiderError {
    HaiderError::new(
        ErrorCode::Internal,
        "session actor stopped before completing append",
        false,
    )
}
