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
//! - **The store is the lag buffer (R12), with TWO distinct overflow
//!   responses.** Replay pages the store (byte-budgeted via
//!   `Store::read_page`); live delivery crosses a bounded per-attachment
//!   catch-up channel — bounded in frames AND estimated bytes ([`publish`])
//!   — and the bounded connection outbox. (a) INTERNAL catch-up overflow is
//!   invisible on the wire: the attachment re-registers in actor order and
//!   resumes from the store, and the client sees only a repeated
//!   `AttachCaughtUp` at a higher head. The catch-up byte bound is HARD —
//!   an oversized envelope is never buffered; it arrives via the same
//!   store resume. (b) Sink-side refusal or lag-under-stall detaches: the
//!   client gets `Lagged` (a control notice on the system reply lane) and
//!   reattaches after its applied cursor. No unbounded queue ever buffers
//!   history for a slow client either way.
//! - **Unknown-id rule.** A client never receives a frame referencing an
//!   attachment id it has not been told about ([`lag_and_detach`] states
//!   the rule and its one interesting case — a detach that outruns the
//!   staged attach response answers the request instead).
//! - **Delivery is paced by atomic sink admission.** Every attachment frame
//!   is admitted through [`FrameSink::offer`] — both dimensions, checked
//!   and consumed in one step — and a busy sink is awaited on a FIFO
//!   admission ticket, so a reading client is never detached by a capacity
//!   race or a page burst and a hot lane cannot starve a queued cold one
//!   ([`deliver_frame`] states the law).
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
    /// channel may retain — a HARD ceiling. Overflow (including a single
    /// envelope larger than the whole budget) takes the exact same
    /// nonblocking lag-then-store-resume transition as a full frame count;
    /// [`publish`] states the rule.
    ///
    /// Aggregate worst case (W3b1 documentation standard), now exact: at the
    /// default caps, `max_attachments` × this budget = 256 × 8 MiB = 2 GiB
    /// of retained catch-up clones, plus up to one
    /// `replay_page_byte_budget` transient per live replay (≤ 256 MiB at the
    /// defaults) — ADDITIVE to the per-connection outbox ceiling stated on
    /// `connection.rs`'s `OutboundLane` (ordinary budget + reply floor +
    /// drain notice; ~1 GiB ordinary at the defaults across 64 connections).
    /// Operators sizing small hosts tune these caps together.
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

/// How a sink answered an [`FrameSink::offer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendAdmission {
    /// The frame was admitted; its frames-and-bytes reservation was granted
    /// and consumed in the same atomic step.
    Sent,
    /// Admitting the frame right now would exceed a bound in EITHER
    /// dimension (frame slots or byte budget), or the attachment's staged
    /// attach response has not been delivered yet. Nothing was consumed; the
    /// caller takes a [`FrameSink::drain_ticket`] and re-offers when it
    /// fires.
    Busy,
    /// The sink can never admit this frame (it is closed, or the frame
    /// exceeds the negotiated frame limit). The caller detaches.
    Refused,
}

/// A nonblocking destination for frames produced by one hub connection.
///
/// The production implementation is the connection's bounded fair outbox.
/// Tests may use a deterministic sink to stop at replay boundaries.
///
/// PAIRING/LIVENESS CONTRACT: a sink whose [`Self::offer`] can answer
/// [`SendAdmission::Busy`] MUST return `Some` from [`Self::drain_ticket`],
/// and every issued ticket must eventually fire (or drop) as capacity frees
/// — otherwise a paced delivery task would wait forever. A sink without
/// tickets must never answer `Busy`; if one does anyway, the hub degrades
/// the answer to a refusal rather than spin or hang.
pub trait FrameSink: Send + Sync {
    /// Admits one complete frame without waiting on a socket.
    fn try_send(&self, frame: WireFrame) -> Result<(), FrameSendError>;

    /// Stages a `SessionAttach` RESPONSE with response-before-event
    /// ordering: a sink with keyed event lanes must not admit this
    /// attachment's event offers until the response has left the queue, and
    /// [`Self::purge_attachment`] must report the response's request id if
    /// it is still staged when the attachment dies. The default is correct
    /// for sinks with one totally-ordered queue and no admission pressure.
    fn try_send_for(
        &self,
        _attachment_id: &AttachmentId,
        frame: WireFrame,
    ) -> Result<(), FrameSendError> {
        self.try_send(frame)
    }

    /// Purges queued traffic for a detached attachment when the sink keeps
    /// keyed lanes, returning the request id of a staged-but-undelivered
    /// attach response so the hub can answer the request instead of emitting
    /// a frame for an attachment id the client never learned. The default is
    /// suitable for sinks without staging queues.
    fn purge_attachment(&self, _attachment_id: &AttachmentId) -> Option<RequestId> {
        None
    }

    /// Atomically admits one frame for this attachment under EVERY bound the
    /// sink enforces (frame slots AND bytes), or reports why it cannot. The
    /// admission is the reservation: it is granted and consumed in one step
    /// under the sink's own lock, so concurrent attachment lanes can never
    /// jointly observe the same headroom and overbook, and there is no
    /// unused grant to release. The default delegates to [`Self::try_send`]
    /// (test sinks keep refusal as their only arbiter).
    fn offer(&self, _attachment_id: &AttachmentId, frame: &WireFrame) -> SendAdmission {
        match self.try_send(frame.clone()) {
            Ok(()) => SendAdmission::Sent,
            Err(FrameSendError) => SendAdmission::Refused,
        }
    }

    /// Enqueues one FIFO admission ticket; the sink fires tickets in arrival
    /// order as capacity frees, so `Busy` waiters are served first-come and
    /// a hot lane cannot systematically leapfrog a queued cold one. `None`
    /// (the default) means the sink never answers `Busy` and refusal is the
    /// only arbiter. See the trait-level pairing/liveness contract.
    fn drain_ticket(&self) -> Option<oneshot::Receiver<()>> {
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
    /// Set ONLY when a cancelled (deadline-forced) shutdown drops its future.
    /// Actors check it after every command receive: a task abort cannot
    /// interrupt an actor resuming from a synchronous boundary, and without
    /// this fence it would start one more store append that nothing will
    /// ever observe. A graceful drain never sets it — queued commands must
    /// complete their persist+publish during the §6.6 grace.
    force_stop: Arc<AtomicBool>,
    device_id: DeviceId,
}

#[derive(Default)]
struct AttachmentSlots {
    total: usize,
    per_connection: HashMap<String, usize>,
}

/// RAII admission slot: refunds itself on drop unless ownership was
/// transferred to the attachments map. Registration can be cancelled at any
/// await (the requesting connection task may be aborted); this guard makes
/// the refund unconditional on every such exit.
struct AttachmentSlotGuard {
    hub: SessionHub,
    connection_id: String,
    armed: bool,
}

impl AttachmentSlotGuard {
    /// Ownership moved into the attachments map; `take_attachment` releases
    /// the slot from now on.
    fn transfer(mut self) {
        self.armed = false;
    }
}

impl Drop for AttachmentSlotGuard {
    fn drop(&mut self) {
        if self.armed {
            self.hub.release_attachment_slot(&self.connection_id);
        }
    }
}

/// Raises the hub-wide forced-stop fence ([`HubInner::force_stop`]) when a
/// shutdown future is dropped before completing gracefully.
struct ForcedStopGuard {
    fence: Arc<AtomicBool>,
    armed: bool,
}

impl Drop for ForcedStopGuard {
    fn drop(&mut self) {
        if self.armed {
            self.fence.store(true, Ordering::Release);
        }
    }
}

/// Join handles that abort every still-owned task if the enclosing shutdown
/// future is itself cancelled at the global drain deadline.
///
/// FENCE-BEFORE-ABORT (by code shape): when live handles are about to be
/// aborted, the hub-wide forced-stop fence is raised FIRST, in the same
/// `Drop` body, so an actor resuming from a synchronous boundary on another
/// worker observes the fence before — never after — its task is aborted and
/// refuses queued work instead of starting an uncancellable store write.
struct OwnedTasks {
    handles: Vec<JoinHandle<()>>,
    force_stop: Arc<AtomicBool>,
}

impl OwnedTasks {
    fn new(handles: Vec<JoinHandle<()>>, force_stop: Arc<AtomicBool>) -> Self {
        Self {
            handles,
            force_stop,
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
        if self.handles.is_empty() {
            // Graceful completion: everything was joined, nothing to abort,
            // the fence stays down.
            return;
        }
        self.force_stop.store(true, Ordering::Release);
        for handle in &self.handles {
            handle.abort();
        }
    }
}

#[derive(Clone)]
struct SessionActorHandle {
    commands: mpsc::Sender<ActorCommand>,
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
                force_stop: Arc::new(AtomicBool::new(false)),
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
        let actor = SessionActorHandle { commands };
        let mut actor_tasks = lock(&self.inner.actor_tasks)?;
        let task = tokio::spawn(run_session_actor(
            session_id.clone(),
            head,
            authority_epoch,
            self.inner.store.worker_generation(),
            self.inner.config.catch_up_byte_budget,
            self.inner.store.clone(),
            Arc::clone(&self.inner.observer),
            Arc::clone(&self.inner.force_stop),
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
        // work. The RAII guard refunds it on EVERY exit — error, cursor
        // rejection, or cancellation at any await inside registration — and
        // is disarmed only once ownership sits in the attachments map, whose
        // `take_attachment` then owns the release. No await separates the
        // owner insertion (the last step of `register_reserved`) from the
        // disarm, so the refund and the map release can never both run.
        if let Some(message) = self.reserve_attachment_slot(connection_id)? {
            return Ok(RegisterResult::Overloaded { message });
        }
        let slot = AttachmentSlotGuard {
            hub: self.clone(),
            connection_id: connection_id.to_owned(),
            armed: true,
        };
        let registered = self
            .register_reserved(connection_id, session_id, after_seq, mode)
            .await;
        if matches!(registered, Ok(RegisterResult::Registered(_))) {
            slot.transfer();
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

    /// Actor-side cleanup after `take_attachment` removed ownership — which
    /// is the authoritative detach. Best effort: a dead actor (forced
    /// teardown) has already dropped its whole attachment map.
    async fn finish_detach(attachment_id: &AttachmentId, owner: AttachmentOwner) {
        let _ = owner
            .actor
            .commands
            .send(ActorCommand::Detach {
                attachment_id: attachment_id.clone(),
            })
            .await;
    }

    async fn detach(&self, attachment_id: &AttachmentId) -> Result<bool, SessionHubError> {
        let owner = self.take_attachment(attachment_id, None)?;
        let Some(owner) = owner else {
            return Ok(false);
        };
        Self::finish_detach(attachment_id, owner).await;
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

    fn offer_attachment(
        &self,
        attachment_id: &AttachmentId,
        sink: &Arc<dyn FrameSink>,
        frame: &WireFrame,
    ) -> SendAdmission {
        let Ok(attachments) = lock(&self.inner.attachments) else {
            return SendAdmission::Refused;
        };
        if !attachments.contains_key(attachment_id) {
            return SendAdmission::Refused;
        }
        // The ownership lock makes admit-vs-detach atomic: detach removes the
        // owner before purging its lane, so no frame can appear after purge.
        sink.offer(attachment_id, frame)
    }

    /// Rejects new hub work synchronously before the runtime announces drain.
    pub fn begin_draining(&self) {
        self.inner.draining.store(true, Ordering::Release);
    }

    /// Begins drain with §6.6's bounded checkpoint grace: new work is
    /// rejected, but commands that already reached their actor complete
    /// their persist AND publish to still-registered attachments, and the
    /// replay tasks stream those final committed envelopes into the
    /// connection sinks before winding down. Only then is attachment
    /// ownership swept. The enclosing barrier deadline bounds the whole
    /// grace: cancelling this future aborts every owned task through the
    /// abort-on-drop guards (the forced path).
    ///
    /// After this method returns, no hub-OWNED task retains the store. One
    /// ledgered exception can outlive it: an already-started blocking store
    /// operation — read, append, or menu CAS, which all share the
    /// `spawn_blocking` adapter — cannot be cancelled and may finish on the
    /// blocking pool afterwards (OPTIMIZATIONS, rider item 6).
    ///
    /// FORCED-PATH LAW: on the forced path an append/CAS may therefore
    /// COMMIT WITHOUT PUBLICATION. That is safe by seq-resume (R9/R11): the
    /// client's next attach replays from its applied cursor and receives the
    /// committed-unpublished envelope then. The forced path is allowed to be
    /// lossy on PUBLICATION, never on durability.
    pub async fn shutdown(&self) -> Result<(), SessionHubError> {
        self.begin_draining();
        // Install the abort-on-drop guards (each raises the forced-stop
        // fence before aborting — see OwnedTasks) plus the standalone fence
        // backstop, all before the first await. If the global drain deadline
        // cancels this future, no hub task is detached and no actor starts
        // another store command.
        let replay_tasks = std::mem::take(&mut *lock(&self.inner.replay_tasks)?);
        let mut replay_tasks = OwnedTasks::new(replay_tasks, Arc::clone(&self.inner.force_stop));
        let actors = {
            let mut actors = lock(&self.inner.actors)?;
            actors.drain().map(|(_, actor)| actor).collect::<Vec<_>>()
        };
        let actor_tasks = std::mem::take(&mut *lock(&self.inner.actor_tasks)?);
        let mut actor_tasks = OwnedTasks::new(actor_tasks, Arc::clone(&self.inner.force_stop));
        // Backstop: dropped-without-disarm covers any cancellation window the
        // task guards cannot see (declared LAST so it drops FIRST).
        let mut forced = ForcedStopGuard {
            fence: Arc::clone(&self.inner.force_stop),
            armed: true,
        };
        self.inner.observer.observe(HubObservation::ShutdownGuarded);
        // GRACE ORDER (P1-3): actors first, gracefully. `Stop` queues behind
        // whatever already reached each actor, so an append or CAS inside its
        // store await commits AND publishes to receivers that are still
        // registered — never to orphaned senders. New commands were already
        // rejected at every hub seam by the draining flag.
        for actor in actors {
            let _ = actor.commands.send(ActorCommand::Stop).await;
        }
        actor_tasks.join_all().await;
        // Actor death drops every catch-up sender; each replay task drains
        // what was already buffered, streams it into its sink, and exits on
        // the closed channel. Join WITHOUT aborting so those final committed
        // envelopes reach the connection outboxes (§6.6's final-broadcast
        // grace; the write side runs under the connection drain deadline).
        replay_tasks.join_all().await;
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
        // Graceful completion: the forced-stop fence stays down.
        forced.armed = false;
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
        // Close-vs-registration sweep (P2-4): `close` sets `closed` BEFORE
        // it snapshots the owners map, so a registration that landed after
        // that snapshot always observes `closed` here and detaches itself;
        // one that landed before it was swept by close. Either way no
        // attachment survives on a closed connection.
        if self.closed.load(Ordering::Acquire) {
            let _ = self.hub.detach(&attachment_id).await;
            return Err(SessionHubError::Closed);
        }
        // Response-before-first-event: the response is staged with a marker
        // that gates this attachment's event offers until it has left the
        // queue, so no replayed event can precede the response that names
        // the attachment id (and a purge that still finds it answers the
        // request — see the unknown-id rule on `lag_and_detach`).
        if self
            .sink
            .try_send_for(
                &attachment_id,
                WireFrame::Response {
                    request_id,
                    body: ResponseBody::SessionAttach {
                        attachment_id: attachment_id.clone(),
                        attach_state,
                    },
                },
            )
            .is_err()
        {
            let _ = self.hub.detach(&attachment_id).await;
            return Err(SessionHubError::Delivery);
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
        // (The purge cannot report a pending response: the client could only
        // name this attachment id after receiving that response.)
        let _ = self.sink.purge_attachment(&attachment_id);
        SessionHub::finish_detach(&attachment_id, owner).await;
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
        // Symmetric with `session_attach` (durable existence precedes actor
        // creation), so a bad session id can never mint a permanent actor.
        // Kept after the attachment-policy check to preserve that check's
        // pinned `capability_denied` for unattached callers.
        if self.hub.inner.store.latest_seq(&session_id).await? == 0 {
            return self.menu_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "session was not found",
                false,
                None,
            );
        }
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
    force_stop: Arc<AtomicBool>,
    mut commands: mpsc::Receiver<ActorCommand>,
) {
    let mut attachments = HashMap::<AttachmentId, ActorAttachment>::new();
    let mut harness = Option::<HarnessHandle>::None;
    // Graceful drain deliberately has NO early-stop here: every command that
    // reached the queue before `Stop` completes its arm, which is what lets
    // an in-flight append/CAS publish during the §6.6 grace. The one fence
    // is the FORCED path ([`HubInner::force_stop`]): a cancelled shutdown
    // must stop an actor resuming from a synchronous boundary from starting
    // one more store command that nothing will observe.
    while let Some(command) = commands.recv().await {
        if force_stop.load(Ordering::Acquire) {
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
/// and credited by the replay task on receive. The byte bound is HARD: an
/// envelope larger than the whole budget takes the same lag path and is
/// delivered by the store resume (`read_page`'s at-least-one-envelope
/// guarantee), never buffered — no lag loop is possible because the resumed
/// head moves past it, and the per-attachment aggregate stays exact.
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
    let mut orphaned = Vec::new();
    for (attachment_id, attachment) in attachments.iter_mut() {
        if !attachment.active {
            continue;
        }
        for (envelope, weight) in envelopes.iter().zip(&weights) {
            let queued = attachment.queued_bytes.load(Ordering::Acquire);
            if queued.saturating_add(*weight) > byte_budget {
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
                    if attachment.events.is_closed() {
                        // The receiver is gone — a registration cancelled
                        // mid-flight or a dead replay task. Nobody can ever
                        // re-register this entry, so remove it instead of
                        // parking it lagged forever.
                        orphaned.push(attachment_id.clone());
                    } else {
                        let _ = attachment.lagged.send(Some(attachment.last_buffered_seq));
                        attachment.active = false;
                    }
                    break;
                }
            }
        }
    }
    for attachment_id in orphaned {
        attachments.remove(&attachment_id);
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
                    None => {
                        // Actor gone (graceful drain) with lag pending: the
                        // committed suffix must still broadcast (§6.6).
                        final_suffix_resume(
                            &hub,
                            &sink,
                            &attachment_id,
                            &session_id,
                            &mut last_sent_seq,
                            &mut registration.lagged,
                            &mut cancel,
                        )
                        .await;
                        break;
                    }
                }
            }
            ReplayStep::Cancelled => break,
            ReplayStep::OutboxFull => {
                lag_and_detach(&hub, &sink, &attachment_id, last_sent_seq).await;
                return;
            }
        }

        // Phase: (after_seq, H] is fully on the wire — announce H.
        hub.inner.observer.observe(HubObservation::BeforeCaughtUp {
            attachment_id: attachment_id.clone(),
            through_seq: high_water,
        });
        let caught_up = WireFrame::AttachCaughtUp {
            attachment_id: attachment_id.clone(),
            high_water_seq: high_water,
        };
        match deliver_frame(
            &hub,
            &sink,
            &attachment_id,
            &caught_up,
            &mut registration.lagged,
            &mut cancel,
        )
        .await
        {
            FrameDelivery::Delivered => {}
            FrameDelivery::Cancelled => return,
            FrameDelivery::Stuck | FrameDelivery::Refused => {
                lag_and_detach(&hub, &sink, &attachment_id, last_sent_seq).await;
                return;
            }
        }
        hub.inner.observer.observe(HubObservation::CaughtUp {
            attachment_id: attachment_id.clone(),
            through_seq: high_water,
        });

        // Phase: buffered drain — deliver `seq > H` already committed during
        // replay, dropping duplicates by seq (at-least-once, R11).
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
                    match deliver_event(
                        &hub,
                        &sink,
                        &attachment_id,
                        &session_id,
                        envelope,
                        &mut last_sent_seq,
                        DeliveryPhase::Buffered,
                        &mut registration.lagged,
                        &mut cancel,
                    )
                    .await
                    {
                        Ok(()) => {}
                        Err(FrameDelivery::Cancelled) => return,
                        Err(_) => {
                            lag_and_detach(&hub, &sink, &attachment_id, last_sent_seq).await;
                            return;
                        }
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
                            None => {
                                final_suffix_resume(
                                    &hub,
                                    &sink,
                                    &attachment_id,
                                    &session_id,
                                    &mut last_sent_seq,
                                    &mut registration.lagged,
                                    &mut cancel,
                                )
                                .await;
                                return;
                            }
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

        // Phase: live — wait on cancellation, the next commit, or lag. The
        // events arm precedes the lag arm deliberately: when the actor dies
        // (graceful drain) the lag watch closes while buffered envelopes may
        // remain, and this order drains that tail — the §6.6 final broadcast
        // — before `recv` returns `None` on the closed channel. A real lag
        // is only raised AFTER the sender stopped buffering, so draining
        // queued items first never delays its handling.
        loop {
            tokio::select! {
                biased;
                changed = cancel.changed() => {
                    if changed.is_err() || *cancel.borrow() {
                        return;
                    }
                }
                queued = registration.events.recv() => {
                    let Some(queued) = queued else {
                        // Channel closed and fully drained (actor gone). A
                        // pending lag means committed envelopes overflowed
                        // past this channel: stream them from the store
                        // before exiting (§6.6 final broadcast).
                        if registration.lagged.borrow().is_some() {
                            final_suffix_resume(
                                &hub,
                                &sink,
                                &attachment_id,
                                &session_id,
                                &mut last_sent_seq,
                                &mut registration.lagged,
                                &mut cancel,
                            )
                            .await;
                        }
                        return;
                    };
                    credit_catch_up(&registration.catch_up_bytes, queued.weight);
                    let envelope = queued.envelope;
                    if envelope.seq <= last_sent_seq {
                        continue;
                    }
                    match deliver_event(
                        &hub,
                        &sink,
                        &attachment_id,
                        &session_id,
                        envelope,
                        &mut last_sent_seq,
                        DeliveryPhase::Live,
                        &mut registration.lagged,
                        &mut cancel,
                    ).await {
                        Ok(()) => {}
                        Err(FrameDelivery::Cancelled) => return,
                        Err(_) => {
                            lag_and_detach(&hub, &sink, &attachment_id, last_sent_seq).await;
                            return;
                        }
                    }
                }
                changed = registration.lagged.changed() => {
                    if changed.is_ok() && registration.lagged.borrow().is_some() {
                        match reregister(&hub, &registration.actor, &attachment_id).await {
                            Some((events, lagged, next_head)) => {
                                registration.events = events;
                                registration.lagged = lagged;
                                high_water = next_head;
                                break;
                            }
                            None => {
                                final_suffix_resume(
                                    &hub,
                                    &sink,
                                    &attachment_id,
                                    &session_id,
                                    &mut last_sent_seq,
                                    &mut registration.lagged,
                                    &mut cancel,
                                )
                                .await;
                                return;
                            }
                        }
                    }
                    // A closed watch (actor gone) loops back into the events
                    // arm, which always resolves on a closed channel — the
                    // pending-lag check there owns the terminal resume.
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

/// Terminal outcome of one paced frame delivery.
enum FrameDelivery {
    Delivered,
    Cancelled,
    /// Lag pressure arrived while the sink was busy: commits are overflowing
    /// the catch-up buffer behind a stalled outbox — the genuinely stuck
    /// client shape. The caller laggs and detaches.
    Stuck,
    /// The sink refused outright (closed, over the negotiated limit, or a
    /// pairing-contract violation). The caller laggs and detaches.
    Refused,
}

/// Delivers one frame under the pacing law.
///
/// PACING LAW (authoritative statement): every attachment frame is admitted
/// through the sink's atomic both-dimension [`FrameSink::offer`] — the
/// admission is the reservation, granted and consumed in one step under the
/// sink's lock, so concurrent lanes cannot race a capacity snapshot and
/// overbook, and a byte-bound sink cannot falsely refuse a reading client.
/// A `Busy` answer makes this task take a FIFO [`FrameSink::drain_ticket`]
/// (taken BEFORE the confirming re-offer, so a unit freed in between still
/// reaches this waiter — no lost wakeup) and park until it fires; tickets
/// fire in arrival order, so waiters are served first-come. Detachment
/// happens only on [`SendAdmission::Refused`] or when lag pressure arrives
/// while the sink is busy ([`FrameDelivery::Stuck`]).
async fn deliver_frame(
    hub: &SessionHub,
    sink: &Arc<dyn FrameSink>,
    attachment_id: &AttachmentId,
    frame: &WireFrame,
    lagged: &mut watch::Receiver<Option<u64>>,
    cancel: &mut watch::Receiver<bool>,
) -> FrameDelivery {
    loop {
        if *cancel.borrow() {
            return FrameDelivery::Cancelled;
        }
        match hub.offer_attachment(attachment_id, sink, frame) {
            SendAdmission::Sent => return FrameDelivery::Delivered,
            SendAdmission::Refused => return FrameDelivery::Refused,
            SendAdmission::Busy => {}
        }
        let Some(ticket) = sink.drain_ticket() else {
            // Pairing-contract violation (see [`FrameSink`]): Busy without a
            // ticket source degrades to refusal instead of spinning.
            return FrameDelivery::Refused;
        };
        // Confirming re-offer AFTER the ticket is queued: capacity freed
        // between the Busy answer and the ticket cannot be lost.
        match hub.offer_attachment(attachment_id, sink, frame) {
            SendAdmission::Sent => return FrameDelivery::Delivered,
            SendAdmission::Refused => return FrameDelivery::Refused,
            SendAdmission::Busy => {}
        }
        // A closed lag watch means the actor is gone (graceful drain): no
        // further commits can pile up, so the stuck signature is impossible
        // and the wait continues on the ticket alone.
        let lag_open = lagged.has_changed().is_ok();
        tokio::pin!(ticket);
        tokio::select! {
            biased;
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    return FrameDelivery::Cancelled;
                }
            }
            changed = lagged.changed(), if lag_open => {
                if changed.is_ok() && lagged.borrow().is_some() {
                    return FrameDelivery::Stuck;
                }
            }
            // Fired or dropped either way means "re-offer now": a dropped
            // ticket comes from a closing sink, whose next offer answers
            // Refused terminally.
            _ = &mut ticket => {}
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
        // A closed lag watch (actor gone, graceful drain) is not a lag: the
        // store replay keeps streaming its range (§6.6 final broadcast) and
        // the later phases exit on the closed channel.
        let lag_open = lagged.has_changed().is_ok();
        let page = tokio::select! {
            biased;
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    return ReplayStep::Cancelled;
                }
                continue;
            }
            changed = lagged.changed(), if lag_open => {
                if changed.is_ok() && lagged.borrow().is_some() {
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
            match deliver_event(
                hub,
                sink,
                attachment_id,
                session_id,
                envelope,
                last_sent_seq,
                DeliveryPhase::Replay,
                lagged,
                cancel,
            )
            .await
            {
                Ok(()) => {}
                Err(FrameDelivery::Cancelled) => return ReplayStep::Cancelled,
                Err(_) => return ReplayStep::OutboxFull,
            }
        }
    }
    ReplayStep::Continue
}

/// Delivers one envelope through [`deliver_frame`]'s pacing law, advancing
/// the cursor only after the sink admitted it.
#[allow(clippy::too_many_arguments)]
async fn deliver_event(
    hub: &SessionHub,
    sink: &Arc<dyn FrameSink>,
    attachment_id: &AttachmentId,
    session_id: &SessionId,
    envelope: RawEnvelope,
    last_sent_seq: &mut u64,
    phase: DeliveryPhase,
    lagged: &mut watch::Receiver<Option<u64>>,
    cancel: &mut watch::Receiver<bool>,
) -> Result<(), FrameDelivery> {
    let seq = envelope.seq;
    hub.inner.observer.observe(HubObservation::BeforeEvent {
        attachment_id: attachment_id.clone(),
        seq,
    });
    let frame = WireFrame::Event {
        attachment_id: attachment_id.clone(),
        session_id: session_id.clone(),
        envelope,
    };
    match deliver_frame(hub, sink, attachment_id, &frame, lagged, cancel).await {
        FrameDelivery::Delivered => {}
        stopped => return Err(stopped),
    }
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

/// Replaces the catch-up channels in actor order after lag/overflow; the
/// actor's `Reregister` arm states the byte-ledger reset rationale.
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

/// Terminal store-resume during graceful drain (§6.6): the actor died with
/// this attachment's lag pending — or its re-registration is no longer
/// possible — so the committed suffix past `last_sent_seq` is streamed from
/// the durable store and closed with a final `AttachCaughtUp` at the durable
/// head. Runs inside the shutdown grace: the barrier deadline bounds it, and
/// a deadline overrun forces the outcome — a committed envelope is delivered
/// here or the drain reports `Forced`, never silently lost.
async fn final_suffix_resume(
    hub: &SessionHub,
    sink: &Arc<dyn FrameSink>,
    attachment_id: &AttachmentId,
    session_id: &SessionId,
    last_sent_seq: &mut u64,
    lagged: &mut watch::Receiver<Option<u64>>,
    cancel: &mut watch::Receiver<bool>,
) {
    let Ok(head) = hub.inner.store.latest_seq(session_id).await else {
        return;
    };
    if head <= *last_sent_seq {
        return;
    }
    let replayed = replay_range(
        hub,
        sink,
        attachment_id,
        session_id,
        last_sent_seq,
        head,
        lagged,
        cancel,
    )
    .await;
    if !matches!(replayed, ReplayStep::Continue) {
        return;
    }
    let caught_up = WireFrame::AttachCaughtUp {
        attachment_id: attachment_id.clone(),
        high_water_seq: head,
    };
    let _ = deliver_frame(hub, sink, attachment_id, &caught_up, lagged, cancel).await;
}

/// UNKNOWN-ID RULE (authoritative statement): a client never receives a
/// frame referencing an attachment id it has not been told about. `Lagged`
/// is a CONTROL notice riding the system reply lane — after the purge,
/// nothing attachment-keyed is ever enqueued again, so a detached lane
/// cannot be recreated. If the purge reports that the staged attach RESPONSE
/// itself never reached the wire, the client has never heard this id at all:
/// the original request is answered with a correlated, retryable error
/// instead.
async fn lag_and_detach(
    hub: &SessionHub,
    sink: &Arc<dyn FrameSink>,
    attachment_id: &AttachmentId,
    last_queued_seq: u64,
) {
    let Ok(Some(owner)) = hub.take_attachment(attachment_id, None) else {
        return;
    };
    match sink.purge_attachment(attachment_id) {
        Some(request_id) => {
            let _ = sink.try_send(WireFrame::Response {
                request_id,
                body: ResponseBody::Error {
                    code: ERROR_CODE_OVERLOADED.into(),
                    message: "attachment overwhelmed before its response was delivered; \
                              re-attach from your applied cursor"
                        .into(),
                    retryable: true,
                    data: None,
                },
            });
        }
        None => {
            let _ = sink.try_send(WireFrame::Lagged {
                attachment_id: attachment_id.clone(),
                last_queued_seq,
            });
        }
    }
    SessionHub::finish_detach(attachment_id, owner).await;
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
