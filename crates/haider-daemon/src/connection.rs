//! Per-connection handshake, ping, bounded writer, and drain notification.
//!
//! Laws at this layer:
//!
//! - peer-UID gate before any byte is read (R2);
//! - `Hello` must be the first frame; anything else is a fatal
//!   `handshake_required`;
//! - outbound frames respect `min(server frame_limit, client
//!   max_receive_frame)` — the daemon never sends what the client said it
//!   cannot receive;
//! - the outbound queue is bounded in BOTH frames and bytes and never blocks
//!   the daemon on a slow client (R12's mechanism): event admission answers
//!   `Busy` and waits its FIFO ticket, replies fall through to the reserved
//!   floor, a reply refused even there is a connection error, and the store —
//!   not the socket — is the lag buffer (laws stated on `OutboundLane` and in
//!   `session_hub`);
//! - `ServerDraining` travels a reserved path outside ordinary frame/byte
//!   capacity. W3b2 deliberately relaxes W3b1's scoped "last frame of this
//!   lane" law: at the next complete-frame boundary the notice is written,
//!   then queued checkpoint envelopes may follow until close or the same
//!   drain deadline (report §6.6 step 10; OPTIMIZATIONS ledger);
//! - a peer that never finishes its handshake is closed at
//!   `handshake_timeout`, so silent peers cannot hold connection slots;
//! - a peer accepted beyond the daemon's connection cap is answered with a
//!   fatal `overloaded` error and closed without ever entering this layer's
//!   task/queue accounting (report §2.5).
//!
//! Session requests and `MenuAnswer` route through the session hub. The
//! negotiated grant ([`ConnectionGrant`]) is retained and every method is
//! authorized centrally there.

// Crate-internal by necessity: these tests exercise the private fair queue and
// writer's reserved drain lane directly. The externally visible session laws
// live in `tests/session_hub_tests.rs` and haider-daemond's UDS tests.
#[cfg(test)]
#[path = "connection_duplex_tests.rs"]
mod connection_duplex_tests;
#[cfg(all(test, unix))]
#[path = "connection_tests.rs"]
mod connection_tests;

use crate::session_hub::{
    AdmissionTicket, FrameSendError, FrameSink, HubConnection, PreparedFrame, SendAdmission,
    SessionHub,
};
use crate::{DaemonError, ShutdownHandle};
use haider_platform::IpcStream;
use haider_rpc::{
    AttachmentId, Capability, CapabilitySet, ERROR_CODE_OVERLOADED, FEATURE_ACCOUNT_LOGIN_API_V1,
    FEATURE_ACCOUNT_OAUTH_IMPORT_SOURCES_V1, FEATURE_ACCOUNT_ROTATION_V1, FEATURE_ARTIFACT_PUT_V1,
    FEATURE_AUTONOMOUS_INTERACTION_V1, FEATURE_BRANCH_CREATE_V1, FEATURE_CHECKPOINT_V1,
    FEATURE_COMMAND_DOOR_V1, FEATURE_COMPACTION_GUARD_V1, FEATURE_CONTEXT_COMPACTION_V1,
    FEATURE_CONVERGENCE_GRAPH_V1, FEATURE_CONVERGENCE_GRAPH_V2, FEATURE_CONVERGENCE_GRAPH_V3,
    FEATURE_CONVERGENCE_GRAPH_V4, FEATURE_EXPORT_SEQ_V1, FEATURE_FALLBACK_CHAIN_V1,
    FEATURE_HAIDER_CODE_PLAN_STATUS_V1, FEATURE_HEADLESS_RUN_V1, FEATURE_HOOKS_SERVER_V1,
    FEATURE_HOOKS_V1, FEATURE_LOOM_AUTHORING_V1, FEATURE_LOOM_CLI_PRESENCE_V1,
    FEATURE_LOOM_PIPE_DAG_V1, FEATURE_LOOM_REGISTRY_ARCHIVE_V1, FEATURE_LOOM_REGISTRY_CAS_V1,
    FEATURE_LOOM_REGISTRY_WATCH_V1, FEATURE_LOOM_V1, FEATURE_LOOM_VALIDATION_V1,
    FEATURE_MODELS_LIST_V1, FEATURE_MONITOR_V1, FEATURE_PIPE_NATIVE_V2,
    FEATURE_PIPE_TOOL_STATUS_V1, FEATURE_PROVIDER_CONFIGURE_V1, FEATURE_PROVIDER_MANAGEMENT_V1,
    FEATURE_PROVIDER_MODELS_V1, FEATURE_PROVIDER_REMOVE_V1,
    FEATURE_RESIDENT_SESSION_BINDING_TOKEN_V1, FEATURE_RESIDENT_SESSION_BINDING_V1,
    FEATURE_RESIDENT_TURN_SUBMIT_V1, FEATURE_RUN_BUDGET_V1, FEATURE_RUN_RETRY_V1,
    FEATURE_SESSION_CONFIG_V1, FEATURE_SESSION_DESCENDANT_STREAM_V1,
    FEATURE_SESSION_FLEET_IDENTITY_V1, FEATURE_SESSION_FLEET_V1, FEATURE_SESSION_FORK_V1,
    FEATURE_SESSION_MUTATION_V1, FEATURE_SESSION_OBSERVE_BATCH_V1, FEATURE_SESSION_OBSERVE_V1,
    FEATURE_SESSION_PERMISSION_OVERRIDES_V1, FEATURE_SESSION_PROMPT_FORK_V1,
    FEATURE_SESSION_RUN_ID_V1, FEATURE_SESSION_WORKFLOW_STATE_V1, FEATURE_SHELL_EXEC_V1,
    FEATURE_SHELL_REGISTRY_V1, FEATURE_SSH_PROFILES_V1, FEATURE_TOOL_INVENTORY_V1,
    FEATURE_TURN_CONTROL_V1, FEATURE_TYPED_AGENT_INSTALL_CANCEL_V1,
    FEATURE_TYPED_AGENT_INSTALL_CONTROL_V1, FEATURE_TYPED_AGENT_INSTALL_V1,
    FEATURE_USER_COMMAND_V1, FEATURE_VAULT_STAGE_V1, FEATURE_WORKFLOW_CATALOG_V1,
    FEATURE_WORKFLOW_GRAPH_V1, Hello, LifecyclePhase, ProtocolError, RequestBody, RequestId,
    ResponseBody, ServerRange, Welcome, WireEncoding, WireFrame, negotiate, uds_codec,
};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::io::IoSlice;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio::time::Instant;
use zeroize::Zeroizing;

/// Registry of writer tasks, owned by the daemon runtime.
///
/// Load-bearing for R17: the connection task cannot own its writer's
/// completion, because a cancelled connection is dropped and `Drop` cannot
/// await. So the `JoinHandle` of every writer goes to `runtime.rs`, which
/// aborts AND JOINS them inside the drain barrier — no writer future, socket
/// half, or payload can be alive when endpoint cleanup and the store close run.
/// The connection keeps only an [`tokio::task::AbortHandle`] (to stop its own
/// writer when it ends early) and a one-shot for the writer's result.
pub(crate) type WriterRegistry = mpsc::UnboundedSender<tokio::task::JoinHandle<()>>;

/// R9 dead-peer policy, server half (client half: `haider-client`):
/// a negotiated peer with no read activity for this long is closed.
pub(crate) const READ_IDLE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(45);
/// R9: queued outbound traffic (Pongs included — they ride the normal
/// bounded/fair accounting) that makes no write progress for this long
/// closes the connection, covering a peer that writes but never reads.
pub(crate) const WRITE_PROGRESS_DEADLINE: std::time::Duration = std::time::Duration::from_secs(45);
/// Cadence of the liveness check; both deadlines are evaluated on this tick.
const LIVENESS_TICK: std::time::Duration = std::time::Duration::from_secs(5);

/// The R9 liveness verdict, kept pure so paused-time unit tests can pin the
/// deadline arithmetic exactly.
///
/// The constants are initial policy, not eternal truth (the ledgered
/// quiescent-stuck-client trigger): instrument no-progress detach and tune
/// from W3c traces.
fn liveness_breach(
    now: Instant,
    last_read: Instant,
    last_write_progress: Instant,
    has_backlog: bool,
) -> Option<&'static str> {
    if now.saturating_duration_since(last_read) >= READ_IDLE_DEADLINE {
        return Some("idle_timeout");
    }
    if has_backlog && now.saturating_duration_since(last_write_progress) >= WRITE_PROGRESS_DEADLINE
    {
        return Some("write_stalled");
    }
    None
}

/// Connection-side handle to the writer: abort authority without join
/// authority (the runtime owns the join).
struct WriterGuard {
    abort: tokio::task::AbortHandle,
    finished: Option<oneshot::Receiver<std::io::Result<bool>>>,
}

impl WriterGuard {
    /// Waits for the writer's own report. An aborted writer reports nothing,
    /// which is exactly "no notice reached the wire".
    async fn finish(&mut self) -> std::io::Result<bool> {
        match self.finished.take() {
            Some(finished) => finished.await.unwrap_or(Ok(false)),
            None => Ok(false),
        }
    }

    fn abort(&self) {
        self.abort.abort();
    }
}

impl Drop for WriterGuard {
    fn drop(&mut self) {
        // Best effort, and a no-op once the writer has finished: a connection
        // that ends early must not leave its writer parked on a peer that
        // never reads. The runtime still joins the task.
        self.abort.abort();
    }
}

/// The reserved drain notice plus the deadline that bounds it and every
/// checkpoint write that follows (R17/W3b2 relaxation).
#[derive(Debug)]
struct ReservedNotice {
    bytes: Vec<u8>,
    deadline: Instant,
}

/// How the drain barrier ended for one connection.
///
/// The runtime uses this to stay honest: a drain that could not put
/// `ServerDraining` on the wire is not a graceful drain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectionExit {
    /// The barrier never reached this connection; it closed on its own first.
    ClosedBeforeDrain { reason: ConnectionRetirementReason },
    /// `ServerDraining` was written and the socket was shut down cleanly.
    NoticeDelivered,
    /// The daemon could not deliver `ServerDraining`: the negotiated frame
    /// limit refused even a truncated notice, or the barrier deadline expired
    /// while this connection was still writing.
    NoticeUndelivered,
}

/// Secret-free reason the runtime journals when one attached client retires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectionRetirementReason {
    PeerProcessExit,
    ConnectionClose,
    Eof,
    Error,
}

impl ConnectionRetirementReason {
    #[must_use]
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::PeerProcessExit => "peer_process_exit",
            Self::ConnectionClose => "connection_close",
            Self::Eof => "eof",
            Self::Error => "error",
        }
    }
}

/// What the handshake granted this connection.
///
/// W3b2 seam: session routing, attach, and menu arbitration authorize against
/// [`ConnectionGrant::capabilities`]; W3b1 already enforces it for the one
/// control frame it accepts (`MenuAnswer`).
#[derive(Debug, Clone)]
pub(crate) struct ConnectionGrant {
    /// Exactly what `negotiate` granted this connection — never re-derived
    /// from the client's request, and never widened afterwards.
    pub(crate) capabilities: CapabilitySet,
    /// Exact additive feature set carried by this peer's Welcome. Tight-frame
    /// withholding must narrow request shapes as well as client affordances.
    pub(crate) features: BTreeSet<String>,
}

/// One fair scheduling lane (policy stated on [`OutboundLane`]). System
/// replies share one lane; each attachment and required ordered stream owns
/// another.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum LaneKey {
    System,
    Attachment(AttachmentId),
    Ordered(String),
}

#[derive(Debug)]
struct QueuedFrame {
    bytes: OutboundBytes,
    /// The JSON Welcome is the encoding barrier: no negotiated MessagePack
    /// frame may pass it on the reserved writer lane.
    welcome: bool,
    /// Set for a staged attach response: popping it clears this attachment's
    /// pending-response marker (response-before-event ordering), and a purge
    /// that still finds it uses the request id to answer the request.
    response_for: Option<(AttachmentId, RequestId)>,
    /// Admitted through the reserved reply floor, outside the ordinary byte
    /// budget; frees the floor when its write settles.
    floor: bool,
}

impl QueuedFrame {
    fn ordinary(bytes: impl Into<OutboundBytes>) -> Self {
        Self {
            bytes: bytes.into(),
            welcome: false,
            response_for: None,
            floor: false,
        }
    }

    fn welcome(bytes: impl Into<OutboundBytes>) -> Self {
        Self {
            bytes: bytes.into(),
            welcome: true,
            response_for: None,
            floor: false,
        }
    }
}

#[derive(Clone)]
pub(crate) enum OutboundBytes {
    Contiguous(Arc<Zeroizing<Vec<u8>>>),
    Framed(Arc<uds_codec::ZeroizingEncodedFrame>),
}

impl OutboundBytes {
    fn len(&self) -> usize {
        match self {
            Self::Contiguous(bytes) => bytes.len(),
            Self::Framed(frame) => frame.framed_len(),
        }
    }
}

impl std::fmt::Debug for OutboundBytes {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("OutboundBytes")
            .field(&format_args!("[REDACTED; {} bytes]", self.len()))
            .finish()
    }
}

impl<const N: usize> PartialEq<&[u8; N]> for OutboundBytes {
    fn eq(&self, other: &&[u8; N]) -> bool {
        matches!(self, Self::Contiguous(bytes) if bytes.as_slice() == other.as_slice())
    }
}

impl From<Vec<u8>> for OutboundBytes {
    fn from(bytes: Vec<u8>) -> Self {
        Self::Contiguous(Arc::new(Zeroizing::new(bytes)))
    }
}

impl From<Zeroizing<Vec<u8>>> for OutboundBytes {
    fn from(bytes: Zeroizing<Vec<u8>>) -> Self {
        Self::Contiguous(Arc::new(bytes))
    }
}

impl From<uds_codec::ZeroizingEncodedFrame> for OutboundBytes {
    fn from(frame: uds_codec::ZeroizingEncodedFrame) -> Self {
        Self::Framed(Arc::new(frame))
    }
}

#[derive(Debug)]
struct OutboundState {
    lanes: HashMap<LaneKey, VecDeque<QueuedFrame>>,
    round_robin: VecDeque<LaneKey>,
    /// Reply frames admitted through the reserved floor; the writer serves
    /// this deque before the fair ring so floor turnover never waits behind
    /// camped event traffic (the drain notice's discipline, applied to
    /// replies).
    floor: VecDeque<QueuedFrame>,
    queued_frames: usize,
    /// Ordinary-budget bytes, including the frame currently being written;
    /// credited only when that write settles, preserving W3b1's real
    /// allocation bound. Floor bytes are accounted separately.
    queued_bytes: usize,
    /// Frames currently admitted through the reply floor (queued or in
    /// flight): at most one.
    floor_in_use: usize,
    /// Sticky once the Welcome encoding barrier has been admitted. The
    /// writer combines this with its local `welcome_written` state.
    welcome_queued: bool,
    /// Attachments whose staged attach response has not yet been popped:
    /// their event offers answer `Busy` until it pops, and a purge that
    /// still finds the marker removes the staged response and reports its
    /// request id so the hub can answer the request instead of emitting a
    /// `Lagged` for an id the client never learned.
    pending_responses: HashMap<AttachmentId, RequestId>,
    /// FIFO admission tickets for `Busy` event offers. The weak token remains
    /// at the head after notification and is removed only by that holder's
    /// admission (or cancellation), so service itself is FIFO: a fresh hot
    /// offer cannot barge before a fired cold waiter, and starvation is
    /// impossible.
    tickets: VecDeque<Weak<Notify>>,
    closed: bool,
}

impl OutboundState {
    /// Drops a dead head prefix and reports whether at least one reservation
    /// was removed. A caller that observes ordinary capacity after pruning
    /// must treat this as a progress event and fire the newly exposed head.
    fn prune_dead_tickets(&mut self) -> bool {
        let mut removed = false;
        while self
            .tickets
            .front()
            .is_some_and(|ticket| ticket.strong_count() == 0)
        {
            self.tickets.pop_front();
            removed = true;
        }
        removed
    }

    fn ticket_is_head(&self, ticket: &AdmissionTicket) -> bool {
        self.tickets
            .front()
            .is_some_and(|head| Weak::ptr_eq(head, &Arc::downgrade(ticket)))
    }

    /// Fires the oldest still-live admission ticket without consuming its
    /// reservation. Called under the state lock whenever capacity frees.
    fn fire_one_ticket(&mut self) {
        self.prune_dead_tickets();
        if let Some(ticket) = self.tickets.front().and_then(Weak::upgrade) {
            ticket.notify_one();
        }
    }

    fn remove_ticket(&mut self, ticket: &AdmissionTicket) -> bool {
        let pruned_dead_head = self.prune_dead_tickets();
        let was_head = self.ticket_is_head(ticket);
        let token = Arc::downgrade(ticket);
        self.tickets
            .retain(|candidate| !Weak::ptr_eq(candidate, &token));
        let pruned_after_removal = self.prune_dead_tickets();
        pruned_dead_head || was_head || pruned_after_removal
    }
}

/// Explicit-close, bounded, fair per-connection outbox.
///
/// FAIRNESS POLICY (authoritative statement; the session hub and tests refer
/// here): frames are keyed into lanes — one `System` lane for replies plus
/// one lane per attachment for events — and the writer drains active lanes
/// round-robin, one frame per visit, so a hot session cannot starve a cold
/// one's drain order. ONE ordinary admission discipline covers every frame:
/// the aggregate frame cap, the per-lane cap (half the frame cap), and the
/// byte budget, exactly W3b1's bounds. On top of it:
///
/// - EVENT offers ([`Self::offer`]) answer `Busy` instead of refusing and
///   wait their turn through the FIFO ticket queue; `Refused` (closed or
///   over the negotiated limit) and lag-under-stall are the only detach
///   shapes, so a reading client is never purged by admission pressure.
/// - REPLIES ([`Self::try_push`]) that the ordinary bounds cannot admit
///   (event traffic may legitimately camp them) fall through to a RESERVED
///   FLOOR of one frame and `frame_limit + 4` bytes, priority-popped by the
///   writer; only a reply that finds the floor occupied too is refused, and
///   that refusal stays terminal (connection-fatal for system frames).
///
/// EXACT WORST CASE, per connection: `outbound_queued_bytes` (ordinary,
/// UDS 4-byte prefixes included in every charge) + `frame_limit + 4` (the
/// reply floor) + one reserved `ServerDraining` notice. Nothing else queues.
///
/// Clones carry enqueue authority but cannot keep the writer alive after the
/// connection owner calls [`Self::close`].
#[derive(Clone)]
struct OutboundLane {
    inner: Arc<OutboundQueue>,
}

struct OutboundQueue {
    state: Mutex<OutboundState>,
    ready: Notify,
    /// Monotonic count of settled writes (R9 write-progress evidence).
    settled: std::sync::atomic::AtomicU64,
    frame_capacity: usize,
    byte_budget: usize,
    per_lane_capacity: usize,
    /// Byte allowance of the reserved reply floor: one maximum encoded frame
    /// (`frame_limit` body + 4-byte UDS prefix), so every legal reply is
    /// admissible when the floor is free — including a full-size
    /// `SessionRead` result.
    floor_byte_allowance: usize,
}

impl OutboundLane {
    fn new(frame_capacity: usize, byte_budget: usize, floor_byte_allowance: usize) -> Self {
        Self {
            inner: Arc::new(OutboundQueue {
                state: Mutex::new(OutboundState {
                    lanes: HashMap::new(),
                    round_robin: VecDeque::new(),
                    floor: VecDeque::new(),
                    queued_frames: 0,
                    queued_bytes: 0,
                    floor_in_use: 0,
                    welcome_queued: false,
                    pending_responses: HashMap::new(),
                    tickets: VecDeque::new(),
                    closed: false,
                }),
                ready: Notify::new(),
                settled: std::sync::atomic::AtomicU64::new(0),
                frame_capacity,
                byte_budget,
                per_lane_capacity: frame_capacity.saturating_add(1).saturating_div(2).max(1),
                floor_byte_allowance,
            }),
        }
    }

    /// Atomically admits one already-encoded EVENT frame under the ordinary
    /// bounds (aggregate frame cap, per-lane cap, byte budget — all
    /// dimensions, one lock) or answers `Busy` without consuming anything.
    /// The admission is the reservation: granted and consumed in one step,
    /// so concurrent lanes can never jointly observe the same headroom and
    /// overbook. An attachment whose staged attach response has not popped
    /// yet is `Busy` regardless of capacity (response-before-event).
    fn offer(
        &self,
        key: LaneKey,
        bytes: impl Into<OutboundBytes>,
        ticket: Option<&AdmissionTicket>,
    ) -> SendAdmission {
        let bytes = bytes.into();
        let charged = bytes.len();
        let Ok(mut state) = self.inner.state.lock() else {
            return SendAdmission::Refused;
        };
        if state.closed {
            return SendAdmission::Refused;
        }
        let pruned_dead_head = state.prune_dead_tickets();
        if pruned_dead_head
            && state.queued_frames < self.inner.frame_capacity
            && state.queued_bytes < self.inner.byte_budget
        {
            // Defense in depth: the RAII holder normally cancels before its
            // weak token can die. If that cleanup is ever bypassed, discovering
            // the dead head while capacity is open must still wake its live
            // successor; no later pop or byte credit may exist.
            state.fire_one_ticket();
        }
        let caller_may_admit =
            state.tickets.is_empty() || ticket.is_some_and(|ticket| state.ticket_is_head(ticket));
        if let LaneKey::Attachment(attachment_id) = &key
            && state.pending_responses.contains_key(attachment_id)
        {
            return SendAdmission::Busy;
        }
        let lane_len = state.lanes.get(&key).map_or(0, VecDeque::len);
        let next_bytes = state.queued_bytes.checked_add(charged);
        if !caller_may_admit
            || state.queued_frames >= self.inner.frame_capacity
            || lane_len >= self.inner.per_lane_capacity
            || next_bytes.is_none_or(|total| total > self.inner.byte_budget)
        {
            return SendAdmission::Busy;
        }
        if let Some(ticket) = ticket
            && state.ticket_is_head(ticket)
        {
            state.tickets.pop_front();
        }
        let activate = lane_len == 0;
        state
            .lanes
            .entry(key.clone())
            .or_default()
            .push_back(QueuedFrame::ordinary(bytes));
        if activate {
            state.round_robin.push_back(key);
        }
        state.queued_frames = state.queued_frames.saturating_add(1);
        state.queued_bytes = state.queued_bytes.saturating_add(charged);
        if state.queued_frames < self.inner.frame_capacity
            && state.queued_bytes < self.inner.byte_budget
        {
            state.fire_one_ticket();
        }
        drop(state);
        self.inner.ready.notify_one();
        SendAdmission::Sent
    }

    /// Enqueues a FIFO admission ticket; the sink fires tickets in order as
    /// capacity frees. Taken BEFORE re-offering (see the hub's
    /// `deliver_frame`) so a unit freed between the answer and the wait
    /// still reaches this waiter — no lost wakeup.
    fn drain_ticket(&self) -> AdmissionTicket {
        let ticket = Arc::new(Notify::new());
        match self.inner.state.lock() {
            Ok(mut state) => {
                if state.closed {
                    // Fire immediately: the next offer answers Refused.
                    ticket.notify_one();
                } else {
                    state.tickets.push_back(Arc::downgrade(&ticket));
                }
            }
            Err(_) => {
                ticket.notify_one();
            }
        }
        ticket
    }

    fn cancel_ticket(&self, ticket: &AdmissionTicket) {
        if let Ok(mut state) = self.inner.state.lock()
            && state.remove_ticket(ticket)
            && state.queued_frames < self.inner.frame_capacity
            && state.queued_bytes < self.inner.byte_budget
        {
            state.fire_one_ticket();
        }
    }

    /// Admits one REPLY frame (responses, protocol errors, pongs, `Lagged`)
    /// through the ordinary bounds, falling through to the reserved floor
    /// (one frame, one maximum encoded reply) when event traffic has the
    /// ordinary capacity camped. Refusal — ordinary full AND floor occupied
    /// — is terminal for the caller.
    fn try_push(&self, key: LaneKey, frame: QueuedFrame) -> Result<(), DaemonError> {
        self.try_push_with_floor(key, frame, true)
    }

    /// Admits ordinary droppable traffic without consuming the reply floor.
    fn try_push_droppable(&self, key: LaneKey, frame: QueuedFrame) -> Result<(), DaemonError> {
        self.try_push_with_floor(key, frame, false)
    }

    fn try_push_with_floor(
        &self,
        key: LaneKey,
        frame: QueuedFrame,
        allow_floor: bool,
    ) -> Result<(), DaemonError> {
        let charged = frame.bytes.len();
        let mut state = self.inner.state.lock().map_err(|_| DaemonError::Task {
            message: "connection outbox mutex is poisoned".into(),
        })?;
        if state.closed {
            return Err(DaemonError::Protocol {
                message: "connection outbox is closed".into(),
            });
        }
        let lane_len = state.lanes.get(&key).map_or(0, VecDeque::len);
        let next_bytes = state.queued_bytes.checked_add(charged);
        let ordinary_admits = state.queued_frames < self.inner.frame_capacity
            && lane_len < self.inner.per_lane_capacity
            && next_bytes.is_some_and(|total| total <= self.inner.byte_budget);
        if ordinary_admits {
            state.welcome_queued |= frame.welcome;
            if let Some((attachment_id, request_id)) = frame.response_for.clone() {
                state.pending_responses.insert(attachment_id, request_id);
            }
            let activate = lane_len == 0;
            state.lanes.entry(key.clone()).or_default().push_back(frame);
            if activate {
                state.round_robin.push_back(key);
            }
            state.queued_frames = state.queued_frames.saturating_add(1);
            state.queued_bytes = state.queued_bytes.saturating_add(charged);
            drop(state);
            self.inner.ready.notify_one();
            return Ok(());
        }
        if allow_floor && state.floor_in_use == 0 && charged <= self.inner.floor_byte_allowance {
            let mut frame = frame;
            state.welcome_queued |= frame.welcome;
            frame.floor = true;
            if let Some((attachment_id, request_id)) = frame.response_for.clone() {
                state.pending_responses.insert(attachment_id, request_id);
            }
            state.floor.push_back(frame);
            state.floor_in_use = 1;
            drop(state);
            self.inner.ready.notify_one();
            return Ok(());
        }
        Err(DaemonError::Protocol {
            message: format!(
                "bounded fair connection outbox unavailable: {charged} more bytes with {} frames and {} bytes queued and the reply floor in use",
                state.queued_frames, state.queued_bytes
            ),
        })
    }

    fn welcome_queued(&self) -> bool {
        self.inner
            .state
            .lock()
            .is_ok_and(|state| state.welcome_queued)
    }

    async fn recv(&self) -> Option<QueuedFrame> {
        loop {
            let notified = self.inner.ready.notified();
            {
                let mut state = match self.inner.state.lock() {
                    Ok(state) => state,
                    Err(_) => return None,
                };
                // The JSON Welcome is a TOTAL encoding barrier. While it is
                // queued, advance only its own FIFO lane (including JSON
                // frames already ahead of it); every floor and other-lane
                // frame may already be MessagePack encoded at admission.
                if state.welcome_queued {
                    let welcome_on_floor = state.floor.front().is_some_and(|frame| frame.welcome);
                    if welcome_on_floor {
                        let Some(frame) = state.floor.pop_front() else {
                            state.closed = true;
                            return None;
                        };
                        finish_pop(&mut state, &frame);
                        return Some(frame);
                    }
                    let welcome_key = state.lanes.iter().find_map(|(key, lane)| {
                        lane.iter().any(|frame| frame.welcome).then(|| key.clone())
                    });
                    let Some(key) = welcome_key else {
                        // A stale marker must fail closed; allowing another
                        // lane to advance could cross the encoding barrier.
                        state.closed = true;
                        return None;
                    };
                    if let Some(position) = state
                        .round_robin
                        .iter()
                        .position(|candidate| candidate == &key)
                    {
                        state.round_robin.remove(position);
                    }
                    let (frame, still_active) = match state.lanes.get_mut(&key) {
                        Some(lane) => {
                            let frame = lane.pop_front();
                            (frame, !lane.is_empty())
                        }
                        None => (None, false),
                    };
                    if still_active {
                        state.round_robin.push_back(key.clone());
                    } else {
                        state.lanes.remove(&key);
                    }
                    let Some(frame) = frame else {
                        state.closed = true;
                        return None;
                    };
                    state.queued_frames = state.queued_frames.saturating_sub(1);
                    finish_pop(&mut state, &frame);
                    return Some(frame);
                }
                // Outside the handshake barrier, the reserved reply floor
                // retains its strict priority over ordinary event lanes.
                if let Some(frame) = state.floor.pop_front() {
                    finish_pop(&mut state, &frame);
                    return Some(frame);
                }
                while let Some(key) = state.round_robin.pop_front() {
                    let (frame, still_active) = match state.lanes.get_mut(&key) {
                        Some(lane) => {
                            let frame = lane.pop_front();
                            (frame, !lane.is_empty())
                        }
                        None => (None, false),
                    };
                    if still_active {
                        state.round_robin.push_back(key.clone());
                    } else {
                        state.lanes.remove(&key);
                    }
                    if let Some(frame) = frame {
                        state.queued_frames = state.queued_frames.saturating_sub(1);
                        finish_pop(&mut state, &frame);
                        return Some(frame);
                    }
                }
                if state.closed {
                    return None;
                }
            }
            notified.await;
        }
    }

    /// Settles one written frame: ordinary bytes credit back to the budget,
    /// a floor frame frees the floor, and the freed capacity fires the next
    /// admission ticket (byte headroom frees HERE, after the write settles —
    /// not at the pop).
    fn credit(&self, frame: &QueuedFrame) {
        self.inner
            .settled
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Ok(mut state) = self.inner.state.lock() {
            if frame.floor {
                state.floor_in_use = 0;
            } else {
                state.queued_bytes = state.queued_bytes.saturating_sub(frame.bytes.len());
            }
            state.fire_one_ticket();
        }
    }

    /// Removes the attachment's event lane. If its staged attach response
    /// has not popped yet, the response is removed too — a client must never
    /// see attach-success for an attachment that already failed — and its
    /// request id is returned so the hub can answer the request instead.
    fn purge(&self, attachment_id: &AttachmentId) -> Option<RequestId> {
        let Ok(mut state) = self.inner.state.lock() else {
            return None;
        };
        let key = LaneKey::Attachment(attachment_id.clone());
        let mut removed = false;
        if let Some(frames) = state.lanes.remove(&key) {
            let count = frames.len();
            let bytes = frames.iter().fold(0_usize, |total, frame| {
                total.saturating_add(frame.bytes.len())
            });
            state.queued_frames = state.queued_frames.saturating_sub(count);
            state.queued_bytes = state.queued_bytes.saturating_sub(bytes);
            removed = count > 0;
        }
        state.round_robin.retain(|candidate| candidate != &key);
        let pending = state.pending_responses.remove(attachment_id);
        if pending.is_some() {
            removed |= splice_staged_response(&mut state, attachment_id);
        }
        if removed {
            state.fire_one_ticket();
        }
        pending
    }

    fn purge_ordered(&self, stream_id: &str) {
        let Ok(mut state) = self.inner.state.lock() else {
            return;
        };
        let key = LaneKey::Ordered(stream_id.to_owned());
        let Some(frames) = state.lanes.remove(&key) else {
            return;
        };
        let count = frames.len();
        let bytes = frames.iter().fold(0_usize, |total, frame| {
            total.saturating_add(frame.bytes.len())
        });
        state.queued_frames = state.queued_frames.saturating_sub(count);
        state.queued_bytes = state.queued_bytes.saturating_sub(bytes);
        state.round_robin.retain(|candidate| candidate != &key);
        if state.queued_frames < self.inner.frame_capacity
            && state.queued_bytes < self.inner.byte_budget
        {
            state.fire_one_ticket();
        }
    }

    /// R9 write-progress marker: changes whenever any queued frame's write
    /// settles.
    fn progress_marker(&self) -> u64 {
        self.inner
            .settled
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Whether ordinary/floor traffic is queued (an in-flight frame counts:
    /// its bytes are only credited when the write settles).
    fn has_backlog(&self) -> bool {
        self.inner
            .state
            .lock()
            .map(|state| {
                state.queued_frames > 0 || state.queued_bytes > 0 || state.floor_in_use > 0
            })
            .unwrap_or(false)
    }

    fn close(&self) {
        if let Ok(mut state) = self.inner.state.lock() {
            state.closed = true;
            // Every waiter re-offers and observes the refusal.
            while let Some(ticket) = state.tickets.pop_front() {
                if let Some(ticket) = ticket.upgrade() {
                    ticket.notify_one();
                }
            }
        }
        self.inner.ready.notify_waiters();
    }

    #[cfg(all(test, unix))]
    fn attachment_lane_count(&self) -> usize {
        self.inner
            .state
            .lock()
            .map(|state| {
                state
                    .lanes
                    .keys()
                    .filter(|key| matches!(key, LaneKey::Attachment(_)))
                    .count()
            })
            .unwrap_or(usize::MAX)
    }
}

/// Pop bookkeeping shared by the floor and the fair ring: clears the
/// pending-response marker when a staged response leaves the queue and fires
/// the next admission ticket for the freed frame slot.
fn finish_pop(state: &mut OutboundState, frame: &QueuedFrame) {
    if let Some((attachment_id, _)) = &frame.response_for {
        state.pending_responses.remove(attachment_id);
    }
    // Ship-gate round: the queued-welcome flag tracks PRESENCE in the queue
    // — the floor deferral and the writer's fire-time barrier both key off
    // it, and both must release the moment the frame moves to the writer.
    if frame.welcome {
        state.welcome_queued = false;
    }
    state.fire_one_ticket();
}

/// Removes a still-queued staged attach response (System lane or floor) for
/// a purged attachment, refunding its accounting.
fn splice_staged_response(state: &mut OutboundState, attachment_id: &AttachmentId) -> bool {
    let mut removed = false;
    if let Some(lane) = state.lanes.get_mut(&LaneKey::System) {
        let before = lane.len();
        let mut refunded = 0_usize;
        lane.retain(|frame| {
            let matches = frame
                .response_for
                .as_ref()
                .is_some_and(|(id, _)| id == attachment_id);
            if matches {
                refunded = refunded.saturating_add(frame.bytes.len());
            }
            !matches
        });
        state.queued_bytes = state.queued_bytes.saturating_sub(refunded);
        let dropped = before.saturating_sub(lane.len());
        state.queued_frames = state.queued_frames.saturating_sub(dropped);
        removed |= dropped > 0;
        if lane.is_empty() {
            state.lanes.remove(&LaneKey::System);
            state
                .round_robin
                .retain(|candidate| candidate != &LaneKey::System);
        }
    }
    let floor_before = state.floor.len();
    state.floor.retain(|frame| {
        frame
            .response_for
            .as_ref()
            .is_none_or(|(id, _)| id != attachment_id)
    });
    if state.floor.len() < floor_before {
        state.floor_in_use = 0;
        removed = true;
    }
    removed
}

struct ConnectionFrameSink {
    lane: OutboundLane,
    outbound_limit: usize,
    encoding: WireEncoding,
    fleet_identity: bool,
}

impl ConnectionFrameSink {
    fn encode(&self, frame: &WireFrame) -> Result<OutboundBytes, DaemonError> {
        if self.fleet_identity {
            return encode_outbound_parts(frame, self.outbound_limit, self.encoding);
        }

        let mut legacy_frame = frame.clone();
        omit_fleet_identity(&mut legacy_frame);
        encode_outbound_parts(&legacy_frame, self.outbound_limit, self.encoding)
    }
}

/// Preserve the pre-X1 payload for a peer whose tight Welcome could not
/// advertise `session_fleet_identity_v1`. Callsigns predate this token; only
/// the two newly gated fields are omitted.
fn omit_fleet_identity(frame: &mut WireFrame) {
    match frame {
        WireFrame::Response {
            body: ResponseBody::SessionFleet { snapshot },
            ..
        } => omit_fleet_node_identity(&mut snapshot.roots),
        WireFrame::Response {
            body: ResponseBody::SessionDescendantsAttach { baseline, .. },
            ..
        } => omit_descendant_node_identity(&mut baseline.roots),
        WireFrame::SessionDescendantStream {
            event: haider_rpc::SessionDescendantStreamEventWire::Delta { child, .. },
            ..
        } => omit_descendant_node_identity(std::slice::from_mut(child)),
        _ => {}
    }
}

fn omit_fleet_node_identity(nodes: &mut [haider_rpc::FleetNodeWire]) {
    for node in nodes {
        node.model = None;
        node.provider = None;
        omit_fleet_node_identity(&mut node.children);
    }
}

fn omit_descendant_node_identity(nodes: &mut [haider_rpc::DescendantStreamNodeWire]) {
    for node in nodes {
        node.model = None;
        node.provider = None;
        omit_descendant_node_identity(&mut node.children);
    }
}

impl FrameSink for ConnectionFrameSink {
    fn max_frame_bytes(&self) -> Option<usize> {
        Some(self.outbound_limit)
    }

    fn close_after_required_delivery_failure(&self) {
        self.lane.close();
    }

    fn try_send(&self, frame: WireFrame) -> Result<(), FrameSendError> {
        let key = attachment_lane(&frame);
        let bytes = self.encode(&frame).map_err(|_| FrameSendError)?;
        self.lane
            .try_push(key, QueuedFrame::ordinary(bytes))
            .map_err(|_| FrameSendError)
    }

    fn try_send_droppable(&self, frame: WireFrame) -> Result<(), FrameSendError> {
        let key = attachment_lane(&frame);
        let bytes = self.encode(&frame).map_err(|_| FrameSendError)?;
        self.lane
            .try_push_droppable(key, QueuedFrame::ordinary(bytes))
            .map_err(|_| FrameSendError)
    }

    fn offer_ordered(&self, stream_id: &str, frame: &WireFrame) -> SendAdmission {
        let Ok(bytes) = self.encode(frame) else {
            return SendAdmission::Refused;
        };
        self.lane
            .offer(LaneKey::Ordered(stream_id.to_owned()), bytes, None)
    }

    fn offer_ordered_prepared(&self, stream_id: &str, frame: &PreparedFrame) -> SendAdmission {
        let Some(bytes) = frame.encoded_bytes() else {
            return frame
                .logical_frame()
                .map_or(SendAdmission::Refused, |frame| {
                    self.offer_ordered(stream_id, frame)
                });
        };
        self.lane
            .offer(LaneKey::Ordered(stream_id.to_owned()), bytes.clone(), None)
    }

    fn offer_ordered_ticketed(
        &self,
        stream_id: &str,
        frame: &WireFrame,
        ticket: &AdmissionTicket,
    ) -> SendAdmission {
        let Ok(bytes) = self.encode(frame) else {
            return SendAdmission::Refused;
        };
        self.lane
            .offer(LaneKey::Ordered(stream_id.to_owned()), bytes, Some(ticket))
    }

    fn offer_ordered_prepared_ticketed(
        &self,
        stream_id: &str,
        frame: &PreparedFrame,
        ticket: &AdmissionTicket,
    ) -> SendAdmission {
        let Some(bytes) = frame.encoded_bytes() else {
            return frame
                .logical_frame()
                .map_or(SendAdmission::Refused, |frame| {
                    self.offer_ordered_ticketed(stream_id, frame, ticket)
                });
        };
        self.lane.offer(
            LaneKey::Ordered(stream_id.to_owned()),
            bytes.clone(),
            Some(ticket),
        )
    }

    fn purge_ordered(&self, stream_id: &str) {
        self.lane.purge_ordered(stream_id);
    }

    fn try_send_for(
        &self,
        attachment_id: &AttachmentId,
        frame: WireFrame,
    ) -> Result<(), FrameSendError> {
        // The attach response rides the SYSTEM reply lane, tagged so its pop
        // releases this attachment's event offers (response-before-event)
        // and a purge can answer the request instead of orphaning it.
        let WireFrame::Response { request_id, .. } = &frame else {
            return self.try_send(frame);
        };
        let marker = Some((attachment_id.clone(), request_id.clone()));
        let bytes = self.encode(&frame).map_err(|_| FrameSendError)?;
        self.lane
            .try_push(
                LaneKey::System,
                QueuedFrame {
                    bytes,
                    welcome: false,
                    response_for: marker,
                    floor: false,
                },
            )
            .map_err(|_| FrameSendError)
    }

    fn purge_attachment(&self, attachment_id: &AttachmentId) -> Option<RequestId> {
        self.lane.purge(attachment_id)
    }

    fn offer(&self, attachment_id: &AttachmentId, frame: &WireFrame) -> SendAdmission {
        let Ok(bytes) = self.encode(frame) else {
            // Exceeds the negotiated frame limit: no amount of draining can
            // ever admit it.
            return SendAdmission::Refused;
        };
        self.lane
            .offer(LaneKey::Attachment(attachment_id.clone()), bytes, None)
    }

    fn prepare(&self, frame: &WireFrame) -> Result<PreparedFrame, FrameSendError> {
        self.encode(frame)
            .map(PreparedFrame::encoded)
            .map_err(|_| FrameSendError)
    }

    fn prepare_event(
        &self,
        attachment_id: &AttachmentId,
        session_id: &haider_protocol::ids::SessionId,
        envelope: &haider_protocol::envelope::RawEnvelope,
    ) -> Result<PreparedFrame, FrameSendError> {
        uds_codec::encode_event_zeroizing_parts_with(
            attachment_id,
            session_id,
            envelope,
            self.outbound_limit,
            self.encoding,
        )
        .map(OutboundBytes::from)
        .map(PreparedFrame::encoded)
        .map_err(|_| FrameSendError)
    }

    fn offer_prepared(&self, attachment_id: &AttachmentId, frame: &PreparedFrame) -> SendAdmission {
        let Some(bytes) = frame.encoded_bytes() else {
            return frame
                .logical_frame()
                .map_or(SendAdmission::Refused, |frame| {
                    self.offer(attachment_id, frame)
                });
        };
        self.lane.offer(
            LaneKey::Attachment(attachment_id.clone()),
            bytes.clone(),
            None,
        )
    }

    fn offer_ticketed(
        &self,
        attachment_id: &AttachmentId,
        frame: &WireFrame,
        ticket: &AdmissionTicket,
    ) -> SendAdmission {
        let Ok(bytes) = self.encode(frame) else {
            return SendAdmission::Refused;
        };
        self.lane.offer(
            LaneKey::Attachment(attachment_id.clone()),
            bytes,
            Some(ticket),
        )
    }

    fn offer_prepared_ticketed(
        &self,
        attachment_id: &AttachmentId,
        frame: &PreparedFrame,
        ticket: &AdmissionTicket,
    ) -> SendAdmission {
        let Some(bytes) = frame.encoded_bytes() else {
            return frame
                .logical_frame()
                .map_or(SendAdmission::Refused, |frame| {
                    self.offer_ticketed(attachment_id, frame, ticket)
                });
        };
        self.lane.offer(
            LaneKey::Attachment(attachment_id.clone()),
            bytes.clone(),
            Some(ticket),
        )
    }

    fn drain_ticket(&self) -> Option<AdmissionTicket> {
        Some(self.lane.drain_ticket())
    }

    fn cancel_ticket(&self, ticket: &AdmissionTicket) {
        self.lane.cancel_ticket(ticket);
    }
}

/// Lane routing: EVENT traffic (events, caught-up markers, and descendant
/// stream frames) is
/// attachment-keyed; everything else — including `Lagged`, which is a
/// CONTROL notice about an attachment that no longer exists — rides the
/// shared System reply lane. After a detach nothing attachment-keyed is ever
/// enqueued again, so a purged lane can never be recreated.
fn attachment_lane(frame: &WireFrame) -> LaneKey {
    match frame {
        WireFrame::Event { attachment_id, .. }
        | WireFrame::AttachCaughtUp { attachment_id, .. }
        | WireFrame::SessionDescendantStream { attachment_id, .. } => {
            LaneKey::Attachment(attachment_id.clone())
        }
        _ => LaneKey::System,
    }
}

/// Payload of the one-shot drain broadcast; becomes the `ServerDraining`
/// frame verbatim (R17).
#[derive(Debug, Clone)]
pub(crate) struct DrainNotice {
    pub(crate) reason: String,
    pub(crate) instance_id: String,
    pub(crate) daemon_generation: u64,
    /// Wire-visible absolute deadline (what the client is told).
    pub(crate) deadline_unix_ms: u64,
    /// The same barrier deadline on the runtime's monotonic clock; this is
    /// what actually bounds this connection's remaining writes (R17).
    pub(crate) deadline: Instant,
}

/// Immutable per-daemon facts shared by every connection task.
#[derive(Clone)]
pub(crate) struct ConnectionContext {
    pub(crate) profile_id: String,
    pub(crate) instance_id: String,
    pub(crate) daemon_generation: u64,
    pub(crate) frame_limit: usize,
    pub(crate) outbound_queue_capacity: usize,
    pub(crate) outbound_queued_bytes: usize,
    /// Admission cap, reported in the `overloaded` rejection message only; the
    /// cap itself is enforced by the accept loop's permit (`runtime.rs`).
    pub(crate) max_connections: usize,
    /// How long a peer may hold a connection slot without completing its
    /// handshake before it is closed.
    pub(crate) handshake_timeout: std::time::Duration,
    /// Where each connection hands its writer task for the runtime to abort
    /// and join at teardown (R17: teardown owns child completion).
    pub(crate) writers: WriterRegistry,
    /// UID that owns the endpoint; every peer must match it (R2).
    pub(crate) owner_uid: u32,
    /// Profile session hub shared by all negotiated connections.
    pub(crate) hub: SessionHub,
    /// Graceful daemon-lifetime control. Only the authenticated Control RPC
    /// below may invoke it from a connection task.
    pub(crate) shutdown: ShutdownHandle,
    /// For error context only; the stream is already accepted.
    pub(crate) endpoint_path: PathBuf,
}

/// Runs one client connection to completion: UID gate, framed read loop,
/// bounded write queue, and a final `ServerDraining` + close when the drain
/// broadcast fires. Errors returned here end only this connection; the
/// [`ConnectionExit`] is what the drain barrier reads to stay honest.
pub(crate) async fn serve(
    stream: IpcStream,
    context: ConnectionContext,
    drain: watch::Receiver<Option<DrainNotice>>,
) -> Result<ConnectionExit, DaemonError> {
    #[cfg(unix)]
    let peer_operation = "read Unix peer credentials";
    #[cfg(windows)]
    let peer_operation = "authenticate Windows named-pipe peer token";
    let (credentials, peer_exit) = haider_platform::peer_credentials_and_exit_watcher(&stream)
        .map_err(|error| DaemonError::io(peer_operation, &context.endpoint_path, error))?;
    if !haider_platform::peer_credentials_are_owner(&credentials, context.owner_uid) {
        #[cfg(unix)]
        let message = format!(
            "refusing peer uid {}, endpoint owner is {}",
            credentials.uid, context.owner_uid
        );
        #[cfg(windows)]
        let message = "refusing named-pipe peer not owned by the daemon user".into();
        return Err(DaemonError::Protocol { message });
    }

    let (reader, writer) = haider_platform::split(stream);
    serve_io_with_peer_exit(reader, writer, context, drain, peer_exit).await
}

/// Runs the framed connection after the transport-specific peer gate.
///
/// Production reaches this only through [`serve`]. Keeping the byte-stream
/// loop generic gives every platform a no-bind `tokio::io::duplex` regression
/// seam without bypassing the real endpoint's owner check.
#[cfg(test)]
async fn serve_io<R, W>(
    reader: R,
    writer: W,
    context: ConnectionContext,
    drain: watch::Receiver<Option<DrainNotice>>,
) -> Result<ConnectionExit, DaemonError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    serve_io_with_peer_exit(reader, writer, context, drain, None).await
}

async fn serve_io_with_peer_exit<R, W>(
    mut reader: R,
    writer: W,
    context: ConnectionContext,
    mut drain: watch::Receiver<Option<DrainNotice>>,
    peer_exit: Option<haider_platform::PeerExitWatcher>,
) -> Result<ConnectionExit, DaemonError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let lane = OutboundLane::new(
        context.outbound_queue_capacity,
        context.outbound_queued_bytes,
        // The reply floor carries exactly one maximum encoded reply:
        // frame_limit body + the 4-byte UDS length prefix.
        context.frame_limit.saturating_add(4),
    );
    // Reserved drain path (R17): one dedicated slot, outside the ordinary
    // queue and outside its byte budget, so no volume of ordinary replies can
    // consume what the last frame needs.
    let (reserve, reserved) = mpsc::channel::<ReservedNotice>(1);
    let writer_lane = lane.clone();
    let (report, reported) = oneshot::channel::<std::io::Result<bool>>();
    let writer_handle = tokio::spawn(async move {
        let outcome = run_writer(writer, writer_lane, reserved).await;
        // The result travels the one-shot; the JoinHandle belongs to the
        // runtime, which owns abort-and-join at teardown.
        let _ = report.send(outcome);
    });
    let mut writer_task = WriterGuard {
        abort: writer_handle.abort_handle(),
        finished: Some(reported),
    };
    if let Err(unowned) = context.writers.send(writer_handle) {
        // The runtime is already gone; nothing would ever join this writer.
        unowned.0.abort();
    }

    // Sensitive inbound path (R7): body buffers are zeroized after
    // deserialize so staged secrets never linger in freed decoder memory.
    let mut decoder = uds_codec::Decoder::new_zeroizing_artifact_put(context.frame_limit);
    let mut buffer = [0_u8; 16 * 1024];
    // Inbound capacity is deliberately one handler: bytes enter through this
    // fixed read buffer and bounded decoder, and every decoded command is
    // completed serially in this connection task. No detached handler set or
    // unbounded inbound command queue exists; cancelling/joining this owned
    // connection task cancels its sole in-flight handler.
    let mut grant = Option::<ConnectionGrant>::None;
    let mut hub_connection = Option::<HubConnection>::None;
    let mut outbound_limit = context.frame_limit;
    let mut encoding = WireEncoding::Json;
    let mut close = false;
    let mut retirement_reason = ConnectionRetirementReason::ConnectionClose;
    let mut notice_reserved = false;
    let mut notice_refused = false;
    // A peer that never speaks must not hold its connection slot forever.
    let handshake_deadline = tokio::time::sleep(context.handshake_timeout);
    tokio::pin!(handshake_deadline);
    // R9 liveness bookkeeping: reads reset the idle deadline; settled writes
    // reset the progress deadline (sampled on the tick).
    let mut last_read = Instant::now();
    let mut progress_marker = lane.progress_marker();
    let mut last_write_progress = Instant::now();
    let mut liveness = tokio::time::interval(LIVENESS_TICK);
    liveness.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    #[cfg(unix)]
    let read_operation = "read Unix connection";
    #[cfg(windows)]
    let read_operation = "read Windows named-pipe connection";
    #[cfg(unix)]
    let write_operation = "write Unix connection";
    #[cfg(windows)]
    let write_operation = "write Windows named-pipe connection";
    let peer_exit = async move {
        match peer_exit {
            Some(watcher) => watcher.wait().await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(peer_exit);

    let processing = async {
        while !close {
            tokio::select! {
                outcome = &mut peer_exit => {
                    let reason = outcome.map_err(|error| {
                        DaemonError::io(
                            "wait for IPC peer process exit",
                            &context.endpoint_path,
                            error,
                        )
                    })?;
                    retirement_reason = match reason {
                        haider_platform::PeerExitReason::ProcessExited => {
                            ConnectionRetirementReason::PeerProcessExit
                        }
                        haider_platform::PeerExitReason::ConnectionClosed => {
                            ConnectionRetirementReason::ConnectionClose
                        }
                    };
                    break;
                }
                changed = drain.changed() => {
                    let notice = changed.is_ok().then(|| drain.borrow().clone()).flatten();
                    if let Some(notice) = notice {
                        match encode_drain_notice(&notice, outbound_limit, encoding) {
                            Ok(bytes) => {
                                // Never blocks and never charged: the reserve exists
                                // precisely so queue pressure cannot lose this frame.
                                notice_reserved = reserve
                                    .try_send(ReservedNotice { bytes, deadline: notice.deadline })
                                    .is_ok();
                            }
                            // Even an empty reason does not fit what this client
                            // said it can receive: the barrier must report that
                            // honestly instead of claiming a clean drain.
                            Err(_) => notice_refused = true,
                        }
                    }
                    break;
                }
                () = &mut handshake_deadline, if grant.is_none() => {
                    let _ = enqueue_fatal(
                        &lane,
                        "handshake_timeout",
                        "Hello did not arrive before the handshake deadline",
                        outbound_limit,
                        encoding,
                    );
                    close = true;
                    retirement_reason = ConnectionRetirementReason::Error;
                }
                _ = liveness.tick(), if grant.is_some() => {
                    let now = Instant::now();
                    let marker = lane.progress_marker();
                    if marker != progress_marker {
                        progress_marker = marker;
                        last_write_progress = now;
                    }
                    if let Some(code) =
                        liveness_breach(now, last_read, last_write_progress, lane.has_backlog())
                    {
                        // Best effort: an idle peer may never read this, and
                        // a stalled writer cannot; the close is the point.
                        let _ = enqueue_fatal(
                            &lane,
                            code,
                            "connection made no ping/read or write progress within the deadline",
                            outbound_limit,
                            encoding,
                        );
                        close = true;
                        retirement_reason = ConnectionRetirementReason::Error;
                    }
                }
                read = reader.read(&mut buffer) => {
                    let read = read.map_err(|error| {
                        DaemonError::io(read_operation, &context.endpoint_path, error)
                    })?;
                    if read == 0 {
                        retirement_reason = ConnectionRetirementReason::Eof;
                        break;
                    }
                    last_read = Instant::now();
                    let mut cursor = 0;
                    let mut decode_error = None;
                    while cursor < read && !close {
                        let step = decoder.push_one(&buffer[cursor..read]);
                        cursor = cursor.saturating_add(step.consumed);
                        if let Some(artifact_put) = step.artifact_put {
                            let Some(connection) = hub_connection.as_ref() else {
                                enqueue_fatal(
                                    &lane,
                                    "handshake_required",
                                    "Hello must be the first frame",
                                    outbound_limit,
                                    WireEncoding::Json,
                                )?;
                                close = true;
                                retirement_reason = ConnectionRetirementReason::Error;
                                continue;
                            };
                            connection
                                .request_decoded_artifact_put(
                                    artifact_put.request_id,
                                    artifact_put.bytes,
                                )
                                .await
                                .map_err(DaemonError::from)?;
                            decoder.set_encoding(encoding);
                        }
                        if let Some(frame) = step.frame {
                            close = handle_frame(
                                frame,
                                &context,
                                &drain,
                                &lane,
                                &mut grant,
                                &mut hub_connection,
                                &mut outbound_limit,
                                &mut encoding,
                            ).await?;
                            if close {
                                retirement_reason = ConnectionRetirementReason::Error;
                            }
                            // `push_one` stops at Prefix{filled:0}; the
                            // negotiated encoding therefore applies to an
                            // unread coalesced suffix, never a partial frame.
                            decoder.set_encoding(encoding);
                        }
                        if let Some(error) = step.error {
                            decode_error = Some(error);
                            break;
                        }
                        if step.consumed == 0 {
                            break;
                        }
                    }
                    if !close
                        && let Some(error) = decode_error
                    {
                        let protocol_error = ProtocolError {
                            code: "invalid_frame".into(),
                            message: error.to_string(),
                            fatal: true,
                            presentation: None,
                            failed_write_ids: Vec::new(),
                        };
                        let _ = enqueue(
                            &lane,
                            &WireFrame::ProtocolError(protocol_error),
                            outbound_limit,
                            encoding,
                        );
                        close = true;
                        retirement_reason = ConnectionRetirementReason::Error;
                    }
                }
            }
        }
        Ok::<(), DaemonError>(())
    }
    .await;

    if let Some(connection) = hub_connection.as_ref() {
        let _ = connection.close().await;
    }
    // Explicit close means cloned attachment senders cannot keep the writer
    // alive. Queued checkpoint traffic drains before socket shutdown.
    lane.close();
    drop(reserve);
    if processing.is_err() {
        // A connection-fatal queue/protocol failure must discard its pending
        // traffic and close, not wait for the very client that overflowed it.
        writer_task.abort();
    }
    let written = writer_task
        .finish()
        .await
        .map_err(|error| DaemonError::io(write_operation, &context.endpoint_path, error))?;
    processing?;
    Ok(match (notice_reserved || notice_refused, written) {
        (false, _) => ConnectionExit::ClosedBeforeDrain {
            reason: retirement_reason,
        },
        (true, true) => ConnectionExit::NoticeDelivered,
        (true, false) => ConnectionExit::NoticeUndelivered,
    })
}

/// Writer half of one connection.
///
/// Ordinary frames are selected fairly and credited after each write. A drain
/// notice arriving mid-frame adopts the deadline without splicing that frame;
/// at its boundary the notice is written, then queued checkpoint traffic may
/// follow under the same deadline. Returns whether `ServerDraining` reached
/// the wire.
async fn run_writer<W>(
    mut writer: W,
    queued: OutboundLane,
    mut reserved: mpsc::Receiver<ReservedNotice>,
) -> std::io::Result<bool>
where
    W: AsyncWrite + Unpin,
{
    let mut notice = Option::<ReservedNotice>::None;
    let mut reserve_open = true;
    let mut notice_written = false;
    let mut welcome_written = false;
    let mut drain_deadline = Option::<Instant>::None;
    loop {
        // A MessagePack drain notice can only be created after negotiation,
        // and negotiation admits its marked JSON Welcome before publishing
        // that encoding to the connection loop. Therefore the sticky queue
        // marker closes the only race: while that barrier is pending, serve
        // the ordinary lanes; after its write, restore the exact reserved
        // bias. Pre-negotiation JSON drains never set the marker and retain
        // their byte-for-byte ordering.
        let welcome_barrier = !welcome_written && queued.welcome_queued();
        let next = if reserve_open && !notice_written && !welcome_barrier {
            tokio::select! {
                biased;
                received = reserved.recv() => {
                    reserve_open = false;
                    notice = received;
                    if notice.is_some() {
                        None
                    } else {
                        queued.recv().await
                    }
                }
                frame = queued.recv() => frame,
            }
        } else {
            queued.recv().await
        };
        if let Some(frame) = next {
            let result = if notice_written {
                match drain_deadline {
                    Some(deadline) => {
                        match tokio::time::timeout_at(
                            deadline,
                            write_outbound(&mut writer, &frame.bytes),
                        )
                        .await
                        {
                            Ok(result) => result,
                            Err(_) => Err(std::io::Error::new(
                                std::io::ErrorKind::TimedOut,
                                "drain deadline expired while writing checkpoint traffic",
                            )),
                        }
                    }
                    None => write_outbound(&mut writer, &frame.bytes).await,
                }
            } else {
                write_ordinary(
                    &mut writer,
                    &frame.bytes,
                    &mut notice,
                    &mut reserve_open,
                    &mut reserved,
                )
                .await
            };
            // Credit after the write settles: an in-flight frame still owns
            // its bytes (or the floor), which is what makes the budgets real
            // memory bounds.
            queued.credit(&frame);
            match result {
                Ok(()) => {
                    welcome_written |= frame.welcome;
                }
                Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
                    return Ok(notice_written);
                }
                Err(error) => return Err(error),
            }
        } else if notice.is_none() {
            break;
        }
        // Ship-gate round: the barrier re-checks at FIRE time — the writer
        // may have parked BEFORE the Welcome was queued, so the pre-select
        // snapshot alone could let a msgpack notice outrun the JSON
        // handshake frame. While the barrier holds, the notice stays parked
        // and the loop keeps serving lanes until the Welcome write flips
        // `welcome_written`.
        let welcome_barrier_now = !welcome_written && queued.welcome_queued();
        if !notice_written
            && !welcome_barrier_now
            && let Some(reserved_notice) = notice.take()
        {
            drain_deadline = Some(reserved_notice.deadline);
            match tokio::time::timeout_at(
                reserved_notice.deadline,
                writer.write_all(&reserved_notice.bytes),
            )
            .await
            {
                Ok(Ok(())) => notice_written = true,
                Ok(Err(error)) => return Err(error),
                Err(_) => return Ok(false),
            }
        }
    }
    if !notice_written {
        let Some(reserved_notice) = notice.take().or(reserved.recv().await) else {
            writer.shutdown().await?;
            return Ok(false);
        };
        match tokio::time::timeout_at(
            reserved_notice.deadline,
            writer.write_all(&reserved_notice.bytes),
        )
        .await
        {
            Ok(Ok(())) => notice_written = true,
            Ok(Err(error)) => return Err(error),
            Err(_) => return Ok(false),
        }
    }
    writer.shutdown().await?;
    Ok(notice_written)
}

/// Writes one ordinary frame, adopting the drain deadline the moment the
/// reserved notice appears.
async fn write_ordinary<W>(
    writer: &mut W,
    bytes: &OutboundBytes,
    notice: &mut Option<ReservedNotice>,
    reserve_open: &mut bool,
    reserved: &mut mpsc::Receiver<ReservedNotice>,
) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let write = write_outbound(writer, bytes);
    tokio::pin!(write);
    loop {
        match notice.as_ref().map(|notice| notice.deadline) {
            Some(deadline) => {
                return match tokio::time::timeout_at(deadline, &mut write).await {
                    Ok(result) => result,
                    Err(_) => Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "drain deadline expired while writing an ordinary frame",
                    )),
                };
            }
            None if *reserve_open => {
                tokio::select! {
                    result = &mut write => return result,
                    received = reserved.recv() => {
                        *reserve_open = false;
                        *notice = received;
                    }
                }
            }
            None => return (&mut write).await,
        }
    }
}

async fn write_outbound<W>(writer: &mut W, bytes: &OutboundBytes) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    match bytes {
        OutboundBytes::Contiguous(bytes) => writer.write_all(bytes.as_slice()).await,
        OutboundBytes::Framed(frame) => {
            let prefix = frame.prefix();
            let body = frame.body();
            let mut prefix_offset = 0;
            let mut body_offset = 0;
            while prefix_offset < prefix.len() || body_offset < body.len() {
                let written = if prefix_offset < prefix.len() {
                    writer
                        .write_vectored(&[
                            IoSlice::new(&prefix[prefix_offset..]),
                            IoSlice::new(&body[body_offset..]),
                        ])
                        .await?
                } else {
                    writer.write(&body[body_offset..]).await?
                };
                if written == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "failed to write outbound frame",
                    ));
                }
                let prefix_remaining = prefix.len().saturating_sub(prefix_offset);
                let prefix_written = written.min(prefix_remaining);
                prefix_offset += prefix_written;
                body_offset += written.saturating_sub(prefix_written);
            }
            Ok(())
        }
    }
}

/// Dispatches one decoded frame. Returns `Ok(true)` when the connection must
/// close (fatal protocol error or rejected handshake).
#[allow(clippy::too_many_arguments)]
async fn handle_frame(
    frame: WireFrame,
    context: &ConnectionContext,
    drain: &watch::Receiver<Option<DrainNotice>>,
    lane: &OutboundLane,
    grant: &mut Option<ConnectionGrant>,
    hub_connection: &mut Option<HubConnection>,
    outbound_limit: &mut usize,
    encoding: &mut WireEncoding,
) -> Result<bool, DaemonError> {
    let Some(_) = grant.as_ref() else {
        let WireFrame::Hello(hello) = frame else {
            enqueue_fatal(
                lane,
                "handshake_required",
                "Hello must be the first frame",
                *outbound_limit,
                WireEncoding::Json,
            )?;
            return Ok(true);
        };
        // From here on the client's max_receive_frame caps everything we
        // send, including the Welcome itself and any rejection.
        *outbound_limit = context.frame_limit.min(hello.max_receive_frame as usize);
        let negotiated_encoding =
            negotiate_hello(hello, context, drain, lane, grant, *outbound_limit)?;
        let close = negotiated_encoding.is_none();
        if let Some(selected) = negotiated_encoding {
            *encoding = selected;
        }
        if !close && let Some(granted) = grant.as_ref() {
            let sink: Arc<dyn FrameSink> = Arc::new(ConnectionFrameSink {
                lane: lane.clone(),
                outbound_limit: *outbound_limit,
                encoding: *encoding,
                fleet_identity: granted.features.contains(FEATURE_SESSION_FLEET_IDENTITY_V1),
            });
            *hub_connection = Some(
                context
                    .hub
                    // UDS with the peer-UID gate already passed: the one
                    // transport allowed to stage raw secrets (R7).
                    .open_connection(
                        granted.capabilities.clone(),
                        sink,
                        crate::accounts::ConnectionTransport::LocalSameUid,
                    )
                    .map_err(DaemonError::from)?,
            );
        }
        return Ok(close);
    };

    match frame {
        WireFrame::Ping { nonce } => {
            enqueue(lane, &WireFrame::Pong { nonce }, *outbound_limit, *encoding)?;
            Ok(false)
        }
        WireFrame::Request {
            request_id,
            body: RequestBody::DaemonShutdown {},
        } => {
            let granted = grant
                .as_ref()
                .is_some_and(|grant| grant.capabilities.contains(&Capability::Control));
            let body = if granted {
                ResponseBody::DaemonShutdown {}
            } else {
                ResponseBody::Error {
                    code: haider_rpc::ERROR_CODE_CAPABILITY_DENIED.into(),
                    message: "daemon.shutdown requires control capability".into(),
                    retryable: false,
                    data: None,
                }
            };
            enqueue(
                lane,
                &WireFrame::Response { request_id, body },
                *outbound_limit,
                *encoding,
            )?;
            if granted {
                // Exactly one request: a second call would select the forced
                // lifecycle path, so fallback decisions remain client-side.
                context
                    .shutdown
                    .request("authenticated daemon.shutdown RPC");
            }
            Ok(false)
        }
        WireFrame::Request { request_id, body } => {
            let required_feature = body.additive_shape_feature();
            if let Some(feature) = required_feature
                && !grant
                    .as_ref()
                    .is_some_and(|grant| grant.features.contains(feature))
            {
                enqueue(
                    lane,
                    &WireFrame::Response {
                        request_id,
                        body: ResponseBody::Error {
                            code: haider_rpc::ERROR_CODE_INVALID_ARGUMENT.into(),
                            message: format!(
                                "request shape requires unadvertised feature `{feature}`"
                            ),
                            retryable: false,
                            data: None,
                        },
                    },
                    *outbound_limit,
                    *encoding,
                )?;
                return Ok(false);
            }
            let connection = hub_connection.as_ref().ok_or_else(|| DaemonError::Task {
                message: "negotiated connection has no session-hub registration".into(),
            })?;
            connection
                .request(request_id, body)
                .await
                .map_err(DaemonError::from)?;
            Ok(false)
        }
        WireFrame::MenuAnswer {
            request_id,
            command_id,
            session_id,
            menu_id,
            request_seq,
            worker_generation,
            option_key,
            option_index,
            input,
        } => {
            let connection = hub_connection.as_ref().ok_or_else(|| DaemonError::Task {
                message: "negotiated connection has no session-hub registration".into(),
            })?;
            connection
                .menu_answer(
                    request_id,
                    command_id,
                    session_id,
                    menu_id,
                    request_seq,
                    worker_generation,
                    option_key,
                    option_index,
                    input,
                )
                .await
                .map_err(DaemonError::from)?;
            Ok(false)
        }
        WireFrame::ResidentSessionBinding {
            session_id,
            worker_generation,
            binding_token,
        } => {
            if !grant
                .as_ref()
                .is_some_and(|grant| grant.capabilities.contains(&Capability::Control))
            {
                enqueue(
                    lane,
                    &WireFrame::ProtocolError(ProtocolError {
                        code: haider_rpc::ERROR_CODE_CAPABILITY_DENIED.into(),
                        message: "resident session binding requires control capability".into(),
                        fatal: false,
                        presentation: None,
                        failed_write_ids: Vec::new(),
                    }),
                    *outbound_limit,
                    *encoding,
                )?;
                return Ok(false);
            }
            let connection = hub_connection.as_ref().ok_or_else(|| DaemonError::Task {
                message: "negotiated connection has no session-hub registration".into(),
            })?;
            connection
                .resident_session_binding(session_id, worker_generation, binding_token)
                .await
                .map_err(DaemonError::from)?;
            Ok(false)
        }
        WireFrame::Unknown => {
            enqueue(
                lane,
                &WireFrame::ProtocolError(ProtocolError {
                    code: "unknown_frame".into(),
                    message: "frame kind is not implemented".into(),
                    fatal: false,
                    presentation: None,
                    failed_write_ids: Vec::new(),
                }),
                *outbound_limit,
                *encoding,
            )?;
            Ok(false)
        }
        _ => {
            enqueue_fatal(
                lane,
                "unexpected_frame",
                "frame is not valid from a connected client",
                *outbound_limit,
                *encoding,
            )?;
            Ok(true)
        }
    }
}

/// Version/capability negotiation via haider-rpc, answered with a `Welcome`
/// carrying instance id, daemon generation, frame limit, and the honest
/// lifecycle phase (`Draining` once the drain broadcast fired, else `Ready` —
/// connections are only accepted between those two states).
fn negotiate_hello(
    hello: Hello,
    context: &ConnectionContext,
    drain: &watch::Receiver<Option<DrainNotice>>,
    lane: &OutboundLane,
    grant: &mut Option<ConnectionGrant>,
    outbound_limit: usize,
) -> Result<Option<WireEncoding>, DaemonError> {
    let server_range = ServerRange {
        protocol_min: haider_rpc::WIRE_PROTOCOL_VERSION,
        protocol_max: haider_rpc::WIRE_PROTOCOL_VERSION,
        capabilities: CapabilitySet::from([Capability::View, Capability::Control]),
        supports_msgpack: true,
    };
    let negotiated = match negotiate(&hello, &server_range) {
        Ok(negotiated) => negotiated,
        Err(error) => {
            enqueue(
                lane,
                &WireFrame::ProtocolError(error),
                outbound_limit,
                WireEncoding::Json,
            )?;
            return Ok(None);
        }
    };
    let frame_limit =
        u32::try_from(context.frame_limit).map_err(|_| DaemonError::InvalidConfig {
            message: "frame limit does not fit the Welcome frame".into(),
        })?;
    let lifecycle_phase = if drain.borrow().is_some() {
        LifecyclePhase::Draining
    } else {
        LifecyclePhase::Ready
    };
    let encoded_welcome = if lifecycle_phase == LifecyclePhase::Ready
        && negotiated.capabilities_granted == server_range.capabilities
        && negotiated.encoding.is_none()
    {
        cached_standard_welcome(context, negotiated.protocol, frame_limit, outbound_limit)?
    } else {
        let welcome = Welcome {
            protocol: negotiated.protocol,
            instance_id: context.instance_id.clone(),
            daemon_generation: context.daemon_generation,
            frame_limit,
            profile_id: context.profile_id.clone(),
            daemon_version: env!("CARGO_PKG_VERSION").into(),
            lifecycle_phase,
            capabilities_granted: negotiated.capabilities_granted.clone(),
            features: welcome_features(),
            user_command_withheld: false,
            encoding: negotiated.encoding.clone(),
        };
        let encoded = encode_welcome_for_peer(welcome, outbound_limit)?;
        AdvertisedWelcome {
            bytes: encoded.bytes.into(),
            features: encoded.features,
        }
    };
    lane.try_push(LaneKey::System, QueuedFrame::welcome(encoded_welcome.bytes))?;
    // Retained, not discarded: the grant is what later frames are authorized
    // against (W3b2 reads it through `ConnectionGrant`).
    *grant = Some(ConnectionGrant {
        capabilities: negotiated.capabilities_granted,
        features: encoded_welcome.features,
    });
    Ok(Some(match negotiated.encoding.as_deref() {
        Some("msgpack") => WireEncoding::MessagePack,
        _ => WireEncoding::Json,
    }))
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct StandardWelcomeCacheKey {
    protocol: u32,
    instance_id: String,
    daemon_generation: u64,
    frame_limit: u32,
    profile_id: String,
}

#[derive(Clone)]
struct AdvertisedWelcome {
    bytes: OutboundBytes,
    features: BTreeSet<String>,
}

struct EncodedWelcome {
    bytes: Zeroizing<Vec<u8>>,
    features: BTreeSet<String>,
}

static STANDARD_WELCOME_CACHE: OnceLock<
    Mutex<HashMap<StandardWelcomeCacheKey, AdvertisedWelcome>>,
> = OnceLock::new();

/// Caches the ordinary Ready + View/Control + JSON Welcome once per daemon.
/// Tight, draining, capability-restricted, and MessagePack-negotiating peers
/// retain the exact per-peer encoder below.
fn cached_standard_welcome(
    context: &ConnectionContext,
    protocol: u32,
    frame_limit: u32,
    outbound_limit: usize,
) -> Result<AdvertisedWelcome, DaemonError> {
    let key = StandardWelcomeCacheKey {
        protocol,
        instance_id: context.instance_id.clone(),
        daemon_generation: context.daemon_generation,
        frame_limit,
        profile_id: context.profile_id.clone(),
    };
    let cache = STANDARD_WELCOME_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = match cache.lock() {
        Ok(cache) => cache,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(welcome) = cache.get(&key)
        && welcome.bytes.len() <= outbound_limit.saturating_add(4)
    {
        return Ok(welcome.clone());
    }
    let welcome = Welcome {
        protocol,
        instance_id: context.instance_id.clone(),
        daemon_generation: context.daemon_generation,
        frame_limit,
        profile_id: context.profile_id.clone(),
        daemon_version: env!("CARGO_PKG_VERSION").into(),
        lifecycle_phase: LifecyclePhase::Ready,
        capabilities_granted: CapabilitySet::from([Capability::View, Capability::Control]),
        features: welcome_features(),
        user_command_withheld: false,
        encoding: None,
    };
    match uds_codec::encode_zeroizing(&WireFrame::Welcome(welcome.clone()), outbound_limit) {
        Ok(encoded) => {
            let advertised = AdvertisedWelcome {
                bytes: OutboundBytes::from(encoded),
                features: welcome.features,
            };
            cache.insert(key, advertised.clone());
            Ok(advertised)
        }
        Err(haider_rpc::CodecError::FrameLimitExceeded { .. }) => {
            drop(cache);
            encode_welcome_for_peer(welcome, outbound_limit).map(|encoded| AdvertisedWelcome {
                bytes: encoded.bytes.into(),
                features: encoded.features,
            })
        }
        Err(error) => Err(DaemonError::Protocol {
            message: format!("outbound frame rejected by peer limit: {error}"),
        }),
    }
}

fn welcome_features() -> BTreeSet<String> {
    BTreeSet::from([
        haider_rpc::FEATURE_AGENT_CANCEL_V1.to_owned(),
        haider_rpc::FEATURE_AGENT_MESSAGE_V1.to_owned(),
        FEATURE_ACCOUNT_LOGIN_API_V1.to_owned(),
        haider_rpc::FEATURE_ACCOUNT_DEVICE_DISCOVERY_V1.to_owned(),
        haider_rpc::FEATURE_ACCOUNT_MANAGEMENT_V1.to_owned(),
        FEATURE_ACCOUNT_OAUTH_IMPORT_SOURCES_V1.to_owned(),
        haider_rpc::FEATURE_ACCOUNT_OAUTH_IMPORT_V1.to_owned(),
        haider_rpc::FEATURE_ACCOUNT_OAUTH_PKCE_V1.to_owned(),
        haider_rpc::FEATURE_ACCOUNT_OAUTH_DEVICE_V1.to_owned(),
        FEATURE_ACCOUNT_ROTATION_V1.to_owned(),
        FEATURE_ARTIFACT_PUT_V1.to_owned(),
        FEATURE_BRANCH_CREATE_V1.to_owned(),
        FEATURE_SESSION_FORK_V1.to_owned(),
        FEATURE_SESSION_PROMPT_FORK_V1.to_owned(),
        FEATURE_COMMAND_DOOR_V1.to_owned(),
        FEATURE_COMPACTION_GUARD_V1.to_owned(),
        FEATURE_CONTEXT_COMPACTION_V1.to_owned(),
        haider_rpc::FEATURE_EFFECT_RECOVERY_V1.to_owned(),
        FEATURE_CONVERGENCE_GRAPH_V1.to_owned(),
        FEATURE_CONVERGENCE_GRAPH_V2.to_owned(),
        FEATURE_CONVERGENCE_GRAPH_V3.to_owned(),
        FEATURE_CONVERGENCE_GRAPH_V4.to_owned(),
        FEATURE_LOOM_V1.to_owned(),
        FEATURE_LOOM_AUTHORING_V1.to_owned(),
        FEATURE_LOOM_PIPE_DAG_V1.to_owned(),
        FEATURE_WORKFLOW_CATALOG_V1.to_owned(),
        FEATURE_WORKFLOW_GRAPH_V1.to_owned(),
        haider_rpc::FEATURE_WORKFLOW_INSTANCE_V1.to_owned(),
        FEATURE_LOOM_CLI_PRESENCE_V1.to_owned(),
        FEATURE_TYPED_AGENT_INSTALL_V1.to_owned(),
        FEATURE_TYPED_AGENT_INSTALL_CONTROL_V1.to_owned(),
        FEATURE_TYPED_AGENT_INSTALL_CANCEL_V1.to_owned(),
        FEATURE_LOOM_REGISTRY_CAS_V1.to_owned(),
        FEATURE_LOOM_REGISTRY_ARCHIVE_V1.to_owned(),
        FEATURE_LOOM_VALIDATION_V1.to_owned(),
        FEATURE_LOOM_REGISTRY_WATCH_V1.to_owned(),
        FEATURE_SESSION_WORKFLOW_STATE_V1.to_owned(),
        FEATURE_SESSION_RUN_ID_V1.to_owned(),
        FEATURE_PIPE_NATIVE_V2.to_owned(),
        FEATURE_PIPE_TOOL_STATUS_V1.to_owned(),
        haider_rpc::FEATURE_INPUT_MIRROR_ATTACHMENTS_V1.to_owned(),
        haider_rpc::FEATURE_INPUT_MIRROR_V1.to_owned(),
        haider_rpc::FEATURE_SESSION_ATTACH_SEALED_V1.to_owned(),
        haider_rpc::FEATURE_SESSION_LIST_WATCH_V1.to_owned(),
        haider_rpc::FEATURE_WIRE_MSGPACK_V1.to_owned(),
        FEATURE_EXPORT_SEQ_V1.to_owned(),
        FEATURE_FALLBACK_CHAIN_V1.to_owned(),
        FEATURE_HEADLESS_RUN_V1.to_owned(),
        FEATURE_HAIDER_CODE_PLAN_STATUS_V1.to_owned(),
        FEATURE_HOOKS_SERVER_V1.to_owned(),
        FEATURE_HOOKS_V1.to_owned(),
        FEATURE_PROVIDER_CONFIGURE_V1.to_owned(),
        FEATURE_PROVIDER_MANAGEMENT_V1.to_owned(),
        haider_rpc::FEATURE_PROVIDER_LOCKDOWN_V1.to_owned(),
        FEATURE_PROVIDER_MODELS_V1.to_owned(),
        FEATURE_MODELS_LIST_V1.to_owned(),
        FEATURE_PROVIDER_REMOVE_V1.to_owned(),
        FEATURE_RESIDENT_SESSION_BINDING_V1.to_owned(),
        FEATURE_RESIDENT_SESSION_BINDING_TOKEN_V1.to_owned(),
        FEATURE_RUN_RETRY_V1.to_owned(),
        FEATURE_RUN_BUDGET_V1.to_owned(),
        haider_rpc::FEATURE_SESSION_EFFORT_SELECT_V1.to_owned(),
        haider_rpc::FEATURE_SESSION_FAST_SELECT_V1.to_owned(),
        haider_rpc::FEATURE_SESSION_MODEL_SELECT_V1.to_owned(),
        FEATURE_SESSION_CONFIG_V1.to_owned(),
        haider_rpc::FEATURE_SESSION_CREATE_ADMISSION_V1.to_owned(),
        FEATURE_SESSION_MUTATION_V1.to_owned(),
        haider_rpc::FEATURE_SESSION_RENAME_V1.to_owned(),
        haider_rpc::FEATURE_SESSION_SEEN_V1.to_owned(),
        haider_rpc::FEATURE_SESSION_NEEDS_INPUT_V1.to_owned(),
        haider_rpc::FEATURE_ACCOUNT_LIST_WATCH_V1.to_owned(),
        haider_rpc::FEATURE_ACCOUNT_LABEL_V1.to_owned(),
        FEATURE_SESSION_FLEET_V1.to_owned(),
        FEATURE_SESSION_FLEET_IDENTITY_V1.to_owned(),
        FEATURE_SESSION_DESCENDANT_STREAM_V1.to_owned(),
        FEATURE_SESSION_OBSERVE_V1.to_owned(),
        FEATURE_SESSION_OBSERVE_BATCH_V1.to_owned(),
        FEATURE_SESSION_PERMISSION_OVERRIDES_V1.to_owned(),
        FEATURE_AUTONOMOUS_INTERACTION_V1.to_owned(),
        haider_rpc::FEATURE_STATUS_SEGMENT_STRUCTURED_V1.to_owned(),
        haider_rpc::FEATURE_STATUS_SEGMENT_V1.to_owned(),
        haider_rpc::FEATURE_STATUS_SNAPSHOT_V1.to_owned(),
        haider_rpc::FEATURE_STORE_HEALTH_V1.to_owned(),
        haider_rpc::FEATURE_TUI_ATTACH_ANNOUNCE_V1.to_owned(),
        haider_rpc::FEATURE_SESSION_AGENT_TYPE_SELECT_V1.to_owned(),
        haider_rpc::FEATURE_SESSION_LINEAGE_V1.to_owned(),
        FEATURE_SHELL_EXEC_V1.to_owned(),
        FEATURE_SSH_PROFILES_V1.to_owned(),
        FEATURE_SHELL_REGISTRY_V1.to_owned(),
        FEATURE_USER_COMMAND_V1.to_owned(),
        FEATURE_TOOL_INVENTORY_V1.to_owned(),
        FEATURE_MONITOR_V1.to_owned(),
        haider_rpc::FEATURE_MONITOR_CONTROL_V1.to_owned(),
        haider_rpc::FEATURE_MONITOR_DELIVERY_V1.to_owned(),
        haider_rpc::FEATURE_TRANSCRIPTION_V1.to_owned(),
        FEATURE_TURN_CONTROL_V1.to_owned(),
        FEATURE_RESIDENT_TURN_SUBMIT_V1.to_owned(),
        haider_rpc::FEATURE_USAGE_REPORT_V1.to_owned(),
        haider_rpc::FEATURE_USAGE_HISTORY_V1.to_owned(),
        haider_rpc::FEATURE_QUEUE_CONTROL_V1.to_owned(),
        haider_rpc::FEATURE_PEER_MESSAGING_V1.to_owned(),
        haider_rpc::FEATURE_COMPUTER_PERMISSION_ACTIONS_V1.to_owned(),
        FEATURE_VAULT_STAGE_V1.to_owned(),
        haider_rpc::FEATURE_ACCOUNT_IDENTITY_V1.to_owned(),
        FEATURE_CHECKPOINT_V1.to_owned(),
    ])
}

/// Encodes the handshake without letting one additive feature make the whole
/// pre-existing connection surface unavailable to a tightly bounded peer.
///
/// New additive tokens are withheld newest-first, restoring the exact
/// preceding Welcome for a peer whose old receive ceiling was already tight.
/// If the older `user_command_v1` token must also be withheld, its shipped
/// `uw` marker remains byte-for-byte unchanged. The retained feature set
/// fences request shapes as well as client affordances.
fn encode_welcome_for_peer(
    mut welcome: Welcome,
    outbound_limit: usize,
) -> Result<EncodedWelcome, DaemonError> {
    loop {
        match uds_codec::encode_zeroizing(&WireFrame::Welcome(welcome.clone()), outbound_limit) {
            Ok(bytes) => {
                return Ok(EncodedWelcome {
                    bytes,
                    features: welcome.features,
                });
            }
            Err(haider_rpc::CodecError::FrameLimitExceeded { .. })
                if welcome.features.remove(haider_rpc::FEATURE_AGENT_CANCEL_V1) => {}
            Err(haider_rpc::CodecError::FrameLimitExceeded { .. })
                if welcome.features.remove(FEATURE_SESSION_FLEET_IDENTITY_V1) => {}
            Err(haider_rpc::CodecError::FrameLimitExceeded { .. })
                if welcome.features.remove(FEATURE_SESSION_PROMPT_FORK_V1) => {}
            Err(haider_rpc::CodecError::FrameLimitExceeded { .. })
                if welcome.features.remove(FEATURE_USER_COMMAND_V1) =>
            {
                welcome.user_command_withheld = true;
            }
            Err(error) => {
                return Err(DaemonError::Protocol {
                    message: format!("outbound frame rejected by peer limit: {error}"),
                });
            }
        }
    }
}

fn enqueue_fatal(
    lane: &OutboundLane,
    code: &str,
    message: &str,
    outbound_limit: usize,
    encoding: WireEncoding,
) -> Result<(), DaemonError> {
    enqueue(
        lane,
        &WireFrame::ProtocolError(ProtocolError {
            code: code.into(),
            message: message.into(),
            fatal: true,
            presentation: None,
            failed_write_ids: Vec::new(),
        }),
        outbound_limit,
        encoding,
    )
}

/// Non-blocking enqueue (R12): a full queue — in frames OR in queued bytes —
/// means the client is not reading its own replies, and the connection errors
/// out instead of stalling the daemon.
fn enqueue(
    lane: &OutboundLane,
    frame: &WireFrame,
    outbound_limit: usize,
    encoding: WireEncoding,
) -> Result<(), DaemonError> {
    let bytes = encode_outbound_parts(frame, outbound_limit, encoding)?;
    lane.try_push(LaneKey::System, QueuedFrame::ordinary(bytes))
}

fn encode_outbound(
    frame: &WireFrame,
    outbound_limit: usize,
    encoding: WireEncoding,
) -> Result<Zeroizing<Vec<u8>>, DaemonError> {
    uds_codec::encode_zeroizing_with(frame, outbound_limit, encoding).map_err(|error| {
        DaemonError::Protocol {
            message: format!("outbound frame rejected by peer limit: {error}"),
        }
    })
}

fn encode_outbound_parts(
    frame: &WireFrame,
    outbound_limit: usize,
    encoding: WireEncoding,
) -> Result<OutboundBytes, DaemonError> {
    uds_codec::encode_zeroizing_parts_with(frame, outbound_limit, encoding)
        .map(OutboundBytes::from)
        .map_err(|error| DaemonError::Protocol {
            message: format!("outbound frame rejected by peer limit: {error}"),
        })
}

/// Encodes `ServerDraining` so it fits what this client said it can receive.
///
/// The public reason is operator prose; the notice is protocol. A reason too
/// long for the negotiated limit is halved until the frame fits rather than
/// costing the connection its last frame (R17). Only a limit that cannot carry
/// even a reasonless notice fails — and that failure is reported, never
/// silently swallowed.
fn encode_drain_notice(
    notice: &DrainNotice,
    outbound_limit: usize,
    encoding: WireEncoding,
) -> Result<Vec<u8>, DaemonError> {
    let mut reason = notice.reason.clone();
    loop {
        let frame = WireFrame::ServerDraining {
            reason: reason.clone(),
            instance_id: notice.instance_id.clone(),
            daemon_generation: notice.daemon_generation,
            deadline_unix_ms: notice.deadline_unix_ms,
        };
        match uds_codec::encode_with(&frame, outbound_limit, encoding) {
            Ok(bytes) => return Ok(bytes),
            Err(error) if reason.is_empty() => {
                return Err(DaemonError::Protocol {
                    message: format!("drain notice does not fit the negotiated limit: {error}"),
                });
            }
            Err(_) => {
                let keep = reason.chars().count() / 2;
                reason = reason.chars().take(keep).collect();
            }
        }
    }
}

/// Answers a peer accepted beyond the connection cap and lets the socket close
/// (report §2.5). Nothing is spawned and nothing is awaited: the rejection goes
/// straight into the fresh socket's empty send buffer, so a connect flood can
/// grow neither tasks nor queues. The peer-UID gate still applies — the daemon
/// does not speak to a peer that does not own its endpoint (R2) — and a socket
/// that somehow refuses the write simply observes the close.
///
/// The write is a direct non-blocking `write(2)` on the accepted descriptor:
/// tokio's `try_write` reports `WouldBlock` on a socket whose writable
/// readiness the reactor has not yet observed, which would silently drop the
/// rejection this function exists to deliver.
pub(crate) fn reject_over_limit(stream: &IpcStream, context: &ConnectionContext) {
    if !haider_platform::peer_is_owner(stream, context.owner_uid).unwrap_or(false) {
        return;
    }
    let frame = WireFrame::ProtocolError(ProtocolError {
        code: ERROR_CODE_OVERLOADED.into(),
        message: format!(
            "daemon is serving its maximum of {} connections; retry later",
            context.max_connections
        ),
        fatal: true,
        presentation: None,
        failed_write_ids: Vec::new(),
    });
    let Ok(bytes) = encode_outbound(&frame, context.frame_limit, WireEncoding::Json) else {
        return;
    };
    haider_platform::write_immediate(stream, &bytes);
}
