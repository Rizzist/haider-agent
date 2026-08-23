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
//! Laws this module obeys but does not own (each has ONE authoritative site):
//!
//! - menu arbitration (first committed answer wins) is stated on
//!   `haider_store::Store::resolve_menu`;
//! - the fair-scheduling policy is stated on `connection.rs`'s
//!   `OutboundLane`;
//! - command-receipt idempotency (R2) is stated on
//!   `haider_store::Store::session_create_receipt`;
//! - the recovery reduction rules (R5) are stated in `turn_recovery.rs`;
//! - the drain barrier order (R9) is stated in `runtime.rs`'s `run_inner`;
//!   this module contributes the admission asymmetry documented on
//!   [`SessionHub::actor_for`] vs `SessionHub::existing_actor`.
//!
//! # Module map (the W3c split; each file opens with its charter)
//!
//! - `mod.rs` — hub state and task ownership, attachment admission ledger,
//!   worker leases, shutdown, [`HubStoreHandle`], and the types every
//!   submodule shares. No command arms, no delivery pacing, no RPC handling.
//! - `actor.rs` — [`run_session_actor`]: the serialized command order that
//!   proves INVARIANTs 1/2 and owns same-generation lease fencing.
//! - `replay.rs` — the per-attachment delivery pipeline; owns the pacing law
//!   and the unknown-id rule.
//! - `rpc.rs` — [`HubConnection`]'s request surface: policy checks, receipt
//!   orchestration, wire error mapping.
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
//! through [`HubStoreHandle::register_harness`] and retain that same
//! constrained handle as their only `StoreHandle`.

#[cfg(test)]
#[path = "../session_hub_private_tests.rs"]
mod session_hub_private_tests;

mod actor;
mod replay;
pub(crate) mod rpc;
#[cfg(test)]
pub(crate) use rpc::pdf_delivery_for_provider;

use crate::DaemonError;
use crate::worker::WorkerManagerHandle;
use actor::run_session_actor;
use async_trait::async_trait;
use haider_core::{
    AbandonedGraph, AcceptedRunRetry, AcceptedShellExec, AcceptedTurn, AppendGroupBatch,
    BranchCreateCommand, BranchCreateOutcome, CacheDiagnosticKey, CancelledTurn,
    ChildGraphAttachCommand, ChildGraphAttachOutcome, ChildTemplateCacheEntry,
    ChildTemplateObservation, ChildTemplateObservationCommand, ComputerEvidenceCommand,
    ComputerEvidenceOutcome, CreatedBranch, CreatedSession, CreatedSessionFork,
    GraphAbandonCommand, GraphAbandonOutcome, GraphEvidenceCommand, GraphEvidenceOutcome,
    GraphFinalizationCommand, GraphFinalizationOutcome, GraphInspectResult, GraphPinCommand,
    GraphPinOutcome, GraphRunSetOpenCommand, GraphRunSetOpenOutcome, GraphSwitchCommand,
    GraphSwitchOutcome, HarnessHandle, MenuResolutionCommand, MenuResolutionOutcome,
    OpenedGraphRunSet, PinnedGraph, ProcessSignalCommand, ProcessSignalOutcome, ProfileStoreFault,
    PromptHistoryCache, RenamedSession, RunRetryCommand, RunRetryOutcome, SeenSession,
    SelectedAgentType, SelectedEffort, SelectedFast, SelectedModel, SessionCreateCommand,
    SessionCreateOutcome, SessionForkCommand, SessionForkOutcome, SessionMetaforkCommit,
    SessionProjectionCheckpoint, SessionRenameCommand, SessionRenameOutcome, SessionSeenCommand,
    SessionSeenOutcome, SessionSelectAgentTypeCommand, SessionSelectAgentTypeOutcome,
    SessionSelectEffortCommand, SessionSelectEffortOutcome, SessionSelectFastCommand,
    SessionSelectFastOutcome, SessionSelectModelCommand, SessionSelectModelOutcome,
    ShellExecAcceptCommand, ShellExecAcceptOutcome, SqliteStoreHandle, StoreHandle, SwitchedGraph,
    TurnAcceptCommand, TurnAcceptOutcome, TurnAdmissionDisposition, TurnCancelCommand,
    TurnCancelOutcome, TurnCancellationStatus,
};
use haider_protocol::EventPayload;
use haider_protocol::branch::BranchDescriptor;
use haider_protocol::envelope::{RawEnvelope, envelope_weight_bytes};
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::ids::{
    BranchId, DeviceId, EventId, GraphId, ItemId, MenuId, RunId, SessionId,
};
use haider_protocol::menu::{
    AnswerVia, EffectRecoveryAction, Menu, MenuAnswer as DurableMenuAnswer, MenuKind,
    effect_recovery_menu,
};
use haider_protocol::state::RunState;
use haider_rpc::{
    ARTIFACT_PUT_MAX_BYTES, AttachMode, AttachState, AttachmentId, CancelStatus, Capability,
    CapabilitySet, CommandId, ERROR_CODE_ALREADY_RESOLVED, ERROR_CODE_ARTIFACT_TOO_LARGE,
    ERROR_CODE_ATTACHMENT_MIME_UNSUPPORTED, ERROR_CODE_ATTACHMENT_NOT_FOUND,
    ERROR_CODE_ATTACHMENT_TOO_LARGE, ERROR_CODE_ATTACHMENTS_TOO_LARGE, ERROR_CODE_BUSY,
    ERROR_CODE_CAPABILITY_DENIED, ERROR_CODE_CURSOR_AHEAD, ERROR_CODE_DRAINING,
    ERROR_CODE_GRAPH_ALREADY_ACTIVE, ERROR_CODE_GRAPH_NOT_ACTIVE, ERROR_CODE_GRAPH_WRONG_NODE,
    ERROR_CODE_INVALID_ARGUMENT, ERROR_CODE_INVALID_CURSOR, ERROR_CODE_NOT_FOUND,
    ERROR_CODE_OVERLOADED, ERROR_CODE_PDF_MALFORMED, ERROR_CODE_PDF_TOO_LARGE,
    ERROR_CODE_PDF_TOO_MANY_PAGES, ERROR_CODE_PROVIDER_MODELS_UNKNOWN, ERROR_CODE_RUN_NOT_ACTIVE,
    ERROR_CODE_STALE_GENERATION, ERROR_CODE_SURFACE_TEXT_TOO_LARGE,
    ERROR_CODE_TOO_MANY_ATTACHMENTS, ERROR_CODE_UNSUPPORTED_SHELL_BUILTIN,
    ERROR_CODE_VISION_UNSUPPORTED, ErrorData, MenuInput, ProtocolError, RequestBody, RequestId,
    ResponseBody, SURFACE_INPUT_MAX_BYTES, SURFACE_STATUS_MAX_BYTES, SeqRange, SessionReadResult,
    SessionSummary, SubmitDisposition, SurfaceInjectOp, SurfaceInputPublishWire, SurfaceInputWire,
    SurfaceStatusPublishWire, SurfaceStatusWire, TodoGraphOpenedWire, WireFrame,
};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::{Notify, broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

#[cfg(test)]
use replay::{FrameDelivery, deliver_frame};
use replay::{ReplayCompletion, run_replay};

const REPLAY_PAGE_SIZE: usize = 256;
const MAX_LIST_PAGE: usize = 100;
const MAX_READ_ENVELOPES: usize = 1_024;

/// Exact image MIME declarations accepted on durable turn submission.
pub const IMAGE_ATTACHMENT_MIME_ALLOWLIST: [&str; 4] =
    ["image/jpeg", "image/png", "image/gif", "image/webp"];

fn profile_store_fault_frame(fault: &ProfileStoreFault) -> WireFrame {
    WireFrame::ProtocolError(ProtocolError {
        code: fault.presentation.subcode.as_str().replace('-', "_"),
        message: fault.presentation.detail.clone(),
        fatal: false,
        presentation: Some(fault.presentation.clone()),
        failed_write_ids: fault.failed_write_ids.clone(),
    })
}

// ────────────────── configuration, sink seam, and observer ──────────────────

/// Bounds for session-actor, attachment-admission, and catch-up traffic.
#[derive(Debug, Clone, Copy)]
pub struct SessionHubConfig {
    /// Commands waiting at one live session actor.
    pub actor_command_capacity: usize,
    /// Commits after `H` retained while one attachment replays (frame count;
    /// `catch_up_byte_budget` bounds the same channel in bytes).
    pub catch_up_capacity: usize,
    /// True-weight units of committed envelopes one attachment's catch-up
    /// channel may retain — a HARD ceiling. [`envelope_weight_bytes`] counts
    /// the full envelope value, every owned ID string, and conservative
    /// payload heap overhead. Overflow (including a single envelope larger
    /// than the whole budget) takes the exact same nonblocking
    /// lag-then-store-resume transition as a full frame count; [`publish`]
    /// states the rule.
    ///
    /// Aggregate catch-up worst case is exactly `max_attachments ×
    /// catch_up_byte_budget` true-weight units: 256 × 8 MiB = 2 GiB at the
    /// defaults. Replay pages are additive and have a different exact bound:
    /// each live replay may materialize `replay_page_byte_budget + one
    /// maximally-sized committed row`. `read_page` guarantees at least one
    /// row for progress even when that row exceeds its page budget, so the
    /// extra row is part of the bound, not hidden inside the nominal 1 MiB.
    /// These are additive to the per-connection outbox ceiling stated on
    /// `connection.rs`'s `OutboundLane`. Operators sizing small hosts tune
    /// these caps together.
    pub catch_up_byte_budget: usize,
    /// True-weight units one replay store page may retain
    /// (`Store::read_page`); the page ends when the budget fills and the next
    /// page resumes from the last delivered sequence.
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

impl SessionHubConfig {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.actor_command_capacity == 0
            || self.catch_up_capacity == 0
            || self.catch_up_byte_budget == 0
            || self.replay_page_byte_budget == 0
        {
            return Err(
                "session hub queue capacities and byte budgets must be greater than zero".into(),
            );
        }
        if self.max_attachments_per_connection == 0 || self.max_attachments == 0 {
            return Err("session hub attachment limits must be greater than zero".into());
        }
        Ok(())
    }
}

const CACHE_DIAGNOSTIC_KEY_FILE: &str = "cache-diagnostic.key";

fn load_or_create_cache_diagnostic_key(
    root: &std::path::Path,
) -> std::io::Result<CacheDiagnosticKey> {
    use std::io::{Read as _, Write as _};

    let path = root.join(CACHE_DIAGNOSTIC_KEY_FILE);
    loop {
        match std::fs::File::open(&path) {
            Ok(mut file) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;

                    if file.metadata()?.permissions().mode() & 0o077 != 0 {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::PermissionDenied,
                            "cache diagnostic key is not owner-only",
                        ));
                    }
                }
                let mut bytes = [0_u8; 32];
                file.read_exact(&mut bytes)?;
                let mut trailing = [0_u8; 1];
                if file.read(&mut trailing)? != 0 {
                    return Err(std::io::Error::other(
                        "cache diagnostic key has an invalid length",
                    ));
                }
                return Ok(CacheDiagnosticKey::from_bytes(bytes));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }

        let mut bytes = [0_u8; 32];
        getrandom::fill(&mut bytes)
            .map_err(|error| std::io::Error::other(format!("generate key: {error}")))?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(mut file) => {
                file.write_all(&bytes)?;
                file.sync_all()?;
                #[cfg(unix)]
                std::fs::File::open(root)?.sync_all()?;
                return Ok(CacheDiagnosticKey::from_bytes(bytes));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
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

/// Opaque identity and reusable wake permit for one FIFO admission waiter.
///
/// The sink retains weak references in its existing waiter queue; pointer
/// identity is the reservation token, so no numeric ticket counter exists.
pub type AdmissionTicket = Arc<Notify>;

/// One logical frame prepared once for repeated outbox admission attempts.
/// Production stores shared encoded bytes; the logical fallback preserves
/// the existing public sink seam for deterministic test implementations.
pub struct PreparedFrame {
    representation: PreparedFrameRepresentation,
}

enum PreparedFrameRepresentation {
    Logical(WireFrame),
    Encoded(crate::connection::OutboundBytes),
}

impl PreparedFrame {
    fn logical(frame: WireFrame) -> Self {
        Self {
            representation: PreparedFrameRepresentation::Logical(frame),
        }
    }

    pub(crate) fn encoded(bytes: crate::connection::OutboundBytes) -> Self {
        Self {
            representation: PreparedFrameRepresentation::Encoded(bytes),
        }
    }

    pub(crate) fn logical_frame(&self) -> Option<&WireFrame> {
        match &self.representation {
            PreparedFrameRepresentation::Logical(frame) => Some(frame),
            PreparedFrameRepresentation::Encoded(_) => None,
        }
    }

    pub(crate) fn encoded_bytes(&self) -> Option<&crate::connection::OutboundBytes> {
        match &self.representation {
            PreparedFrameRepresentation::Logical(_) => None,
            PreparedFrameRepresentation::Encoded(bytes) => Some(bytes),
        }
    }
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

    /// Closes a production transport after it refuses authoritative state.
    /// Reconnection then receives the retained baseline instead of silently
    /// continuing with stale state. Deterministic test sinks may leave this a
    /// no-op unless the close behavior itself is under test.
    fn close_after_required_delivery_failure(&self) {}

    /// The largest encodable outbound frame, when the sink knows one.
    /// Handlers whose replies have a KNOWN maximum size (surface snapshots)
    /// refuse registration upfront instead of letting a later oversized
    /// encode kill the connection (rev933c finding 8). `None` = ungated.
    fn max_frame_bytes(&self) -> Option<usize> {
        None
    }

    /// Admits best-effort traffic without borrowing capacity reserved for
    /// replies or pongs. Sinks without a separate reply floor may use their
    /// normal admission path.
    fn try_send_droppable(&self, frame: WireFrame) -> Result<(), FrameSendError> {
        self.try_send(frame)
    }

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

    /// Prepares one frame before the pacing loop. Bounded production sinks
    /// encode here once; logical test sinks retain their existing behavior.
    fn prepare(&self, frame: &WireFrame) -> Result<PreparedFrame, FrameSendError> {
        Ok(PreparedFrame::logical(frame.clone()))
    }

    /// Borrowed EVENT preparation avoids cloning the durable envelope solely
    /// to serialize it. The default keeps compatibility for custom sinks.
    fn prepare_event(
        &self,
        attachment_id: &AttachmentId,
        session_id: &SessionId,
        envelope: &RawEnvelope,
    ) -> Result<PreparedFrame, FrameSendError> {
        self.prepare(&WireFrame::Event {
            attachment_id: attachment_id.clone(),
            session_id: session_id.clone(),
            envelope: envelope.clone(),
        })
    }

    fn offer_prepared(&self, attachment_id: &AttachmentId, frame: &PreparedFrame) -> SendAdmission {
        frame
            .logical_frame()
            .map_or(SendAdmission::Refused, |frame| {
                self.offer(attachment_id, frame)
            })
    }

    /// Re-offers with the reservation token returned by
    /// [`Self::drain_ticket`]. A sink that can answer `Busy` must admit
    /// ordinary attachment traffic only when its waiter queue is empty or
    /// this token identifies the head waiter. The default suits sinks that
    /// never answer `Busy`.
    fn offer_ticketed(
        &self,
        attachment_id: &AttachmentId,
        frame: &WireFrame,
        _ticket: &AdmissionTicket,
    ) -> SendAdmission {
        self.offer(attachment_id, frame)
    }

    fn offer_prepared_ticketed(
        &self,
        attachment_id: &AttachmentId,
        frame: &PreparedFrame,
        ticket: &AdmissionTicket,
    ) -> SendAdmission {
        frame
            .logical_frame()
            .map_or(SendAdmission::Refused, |frame| {
                self.offer_ticketed(attachment_id, frame, ticket)
            })
    }

    /// Enqueues one FIFO admission ticket. Capacity wakes the head without
    /// consuming its reservation; only successful admission removes it, so
    /// a fresh offer cannot barge between notification and service. `None`
    /// (the default) means the sink never answers `Busy` and refusal is the
    /// only arbiter. See the trait-level pairing/liveness contract.
    fn drain_ticket(&self) -> Option<AdmissionTicket> {
        None
    }

    /// Removes an unconsumed ticket when delivery is cancelled or refused.
    /// A bounded sink must wake the next live head if this token owned it.
    fn cancel_ticket(&self, _ticket: &AdmissionTicket) {}

    /// Unit-test-only pause inserted after a ticket fires and before the
    /// confirming re-offer. Production sinks never expose this hook.
    #[cfg(test)]
    fn ticket_fired_test_gate(
        &self,
    ) -> Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>>> {
        None
    }
}

/// Cancellation-safe ownership of one unconsumed admission ticket.
///
/// `deliver_frame` can be raw-aborted at any await. Retaining the ticket only
/// through this guard makes every such drop run the sink's normal cancellation
/// path, which removes the token and wakes its successor when it owned the
/// queue head.
struct AdmissionTicketGuard {
    sink: Arc<dyn FrameSink>,
    ticket: AdmissionTicket,
    armed: bool,
}

impl AdmissionTicketGuard {
    fn new(sink: Arc<dyn FrameSink>, ticket: AdmissionTicket) -> Self {
        Self {
            sink,
            ticket,
            armed: true,
        }
    }

    fn ticket(&self) -> &AdmissionTicket {
        &self.ticket
    }

    /// The sink consumed the token atomically with successful admission.
    fn disarm(&mut self) {
        self.armed = false;
    }

    /// Orderly refusal/cancellation uses the same path as an asynchronous
    /// future drop, then disarms the guard to prevent a second cancellation.
    fn cancel(&mut self) {
        if self.armed {
            self.armed = false;
            self.sink.cancel_ticket(&self.ticket);
        }
    }
}

impl Drop for AdmissionTicketGuard {
    fn drop(&mut self) {
        self.cancel();
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
    /// After ordered `Stop` delivery, every session actor task has ended and
    /// dropped its attachment senders; replay tasks now solely own the final
    /// broadcast grace.
    ShutdownActorsStopped,
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
    FinalSuffixHeadCaptured {
        attachment_id: AttachmentId,
        head: u64,
    },
}

/// Optional boundary observer. Implementations must return promptly outside
/// deterministic tests.
pub trait SessionHubObserver: Send + Sync {
    fn observe(&self, observation: HubObservation);
}

/// Monotonic W3c operational counters for the store-as-lag-buffer path.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionHubMetrics {
    pub catch_up_overflows: u64,
    pub discarded_envelopes: u64,
    pub discarded_store_pages: u64,
    pub store_resumes: u64,
    pub reregistrations: u64,
    pub outbox_detaches: u64,
}

#[derive(Default)]
struct HubMetrics {
    catch_up_overflows: AtomicU64,
    discarded_envelopes: AtomicU64,
    discarded_store_pages: AtomicU64,
    store_resumes: AtomicU64,
    reregistrations: AtomicU64,
    outbox_detaches: AtomicU64,
}

impl HubMetrics {
    fn snapshot(&self) -> SessionHubMetrics {
        SessionHubMetrics {
            catch_up_overflows: self.catch_up_overflows.load(Ordering::Relaxed),
            discarded_envelopes: self.discarded_envelopes.load(Ordering::Relaxed),
            discarded_store_pages: self.discarded_store_pages.load(Ordering::Relaxed),
            store_resumes: self.store_resumes.load(Ordering::Relaxed),
            reregistrations: self.reregistrations.load(Ordering::Relaxed),
            outbox_detaches: self.outbox_detaches.load(Ordering::Relaxed),
        }
    }
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
    append_committer: AppendCommitter,
    append_commit_task: Mutex<Option<JoinHandle<()>>>,
    pipe_native: Arc<crate::pipe_native::PipeNativeWriter>,
    config: SessionHubConfig,
    observer: Arc<dyn SessionHubObserver>,
    metrics: Arc<HubMetrics>,
    actors: Mutex<HashMap<SessionId, SessionActorHandle>>,
    /// Connection-level unsolicited sinks. Store failures fan out here, and
    /// volatile input injection uses the current publisher's indexed route.
    diagnostic_sinks: Mutex<HashMap<String, Arc<dyn FrameSink>>>,
    /// Latest profile-level resident-TUI binding and its current publisher.
    /// The retained value is the late-subscriber/reconnect baseline; owner
    /// identity prevents an overlapped old connection from clearing its
    /// replacement's same-generation announcement.
    resident_binding: Mutex<ResidentBindingRegistry>,
    /// Connections authorized to observe session identity. Kept separate
    /// from the general diagnostic sink index because profile-health signals
    /// intentionally reach capability-empty peers too.
    resident_binding_viewers: Mutex<HashSet<String>>,
    /// Daemon-generation-only composer/status truth. This state is never
    /// journaled or projected into prompts; clients republish after restart.
    surfaces: Mutex<HashMap<SessionId, SessionSurfaceState>>,
    /// Coalescing wake for surface watchers. Per-session change generations
    /// remain the delivery/deduplication authority; this only replaces the
    /// 50ms discovery delay on the common path.
    surface_publications: watch::Sender<u64>,
    /// Permanent tombstones for sessions deleted in this daemon lifetime.
    /// `actor_for` checks them at both sides of its await so deletion cannot
    /// race actor recreation or fresh admission.
    deleting_sessions: Mutex<HashSet<SessionId>>,
    actor_tasks: Mutex<Vec<JoinHandle<()>>>,
    replay_tasks: Mutex<Vec<JoinHandle<ReplayCompletion>>>,
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
    cache_diagnostic_key: CacheDiagnosticKey,
    worker_manager: Mutex<Option<WorkerManagerHandle>>,
    accounts: Mutex<Option<crate::accounts::AccountsFacade>>,
    creatable_providers: Mutex<Option<std::collections::BTreeSet<String>>>,
    hooks: Arc<Mutex<Option<crate::hooks::WeakHookService>>>,
    /// One post-commit fan-out shared by every session actor. The journal is
    /// still authoritative: the observe fold rebuilds on miss/gap, while the
    /// roster channel is only a coalescing wake carrying the dirty session.
    commit_projection: Arc<CommitProjection>,
    observe_digests: Arc<rpc::ObserveDigestCache>,
    roster_publications: broadcast::Sender<SessionId>,
    /// Coalescing wake for the Haider Code plan poller. The generation moves
    /// only when attachment interest or a committed provider/model selection
    /// changes; ordinary turn traffic cannot wake an unauthorized account
    /// into a retry loop.
    haider_code_plan_changes: watch::Sender<u64>,
    usage_report: Mutex<Option<Arc<crate::usage_report::UsageReportService>>>,
    /// W-A: the ONE in-memory projection of every session's background
    /// tasks. Hub-owned so every facade clone shares it; the journal's
    /// task facts remain the durable truth.
    tasks: crate::tasks::TaskRegistry,
    /// W-B: session-scoped web-capability degrades (anthropic server tools
    /// 400ed → local fallback; codex alpha/search 404/410 → stop advertising
    /// the client search). Deliberately IN-MEMORY: "for the session" is a
    /// runtime scope — a daemon restart retries the capability once.
    web_degrade: Mutex<HashMap<SessionId, crate::worker::WebCapabilityDegrade>>,
    /// Ephemeral compiled-prompt acceleration. Journal bytes remain the
    /// authority; the cache is discarded with this daemon generation.
    prompt_history: PromptHistoryCache,
}

#[derive(Default)]
struct ResidentBindingRegistry {
    next_revision: u64,
    publishers: HashMap<String, ResidentBindingState>,
    /// Retained explicit unbind after the last publisher exits, so a client
    /// reconnecting after required-delivery refusal receives the clearing
    /// baseline rather than relying on local reset behavior.
    vacant_generation: Option<u64>,
}

impl ResidentBindingRegistry {
    fn current(&self) -> Option<(&str, &ResidentBindingState)> {
        self.publishers
            .iter()
            .max_by_key(|(_, binding)| binding.revision)
            .map(|(owner, binding)| (owner.as_str(), binding))
    }

    fn visible(&self) -> Option<(Option<SessionId>, u64, Option<String>)> {
        self.current()
            .map(|(_, binding)| {
                (
                    binding.session_id.clone(),
                    binding.worker_generation,
                    binding.binding_token.clone(),
                )
            })
            .or_else(|| {
                self.vacant_generation
                    .map(|generation| (None, generation, None))
            })
    }
}

struct ResidentBindingState {
    session_id: Option<SessionId>,
    worker_generation: u64,
    binding_token: Option<String>,
    revision: u64,
}

pub(super) struct CommitProjection {
    hooks: Arc<Mutex<Option<crate::hooks::WeakHookService>>>,
    observe_digests: Arc<rpc::ObserveDigestCache>,
    roster_publications: broadcast::Sender<SessionId>,
    haider_code_plan_changes: watch::Sender<u64>,
}

impl CommitProjection {
    pub(super) fn observe_committed(&self, envelopes: &[RawEnvelope]) {
        self.observe_digests.observe_committed(envelopes);
        if let Ok(installed) = self.hooks.lock()
            && let Some(hooks) = installed
                .as_ref()
                .and_then(crate::hooks::WeakHookService::upgrade)
        {
            hooks.observe_committed(envelopes);
        }
        if let Some(envelope) = envelopes.last() {
            let _ = self.roster_publications.send(envelope.session_id.clone());
        }
        if envelopes.iter().any(|envelope| {
            haider_protocol::session::ModelSelected::from_payload_value(&envelope.payload).is_some()
        }) {
            self.haider_code_plan_changes
                .send_modify(|generation| *generation = generation.wrapping_add(1));
        }
    }
}

#[derive(Default)]
struct SessionSurfaceState {
    input: Option<SurfaceInputWire>,
    status: Option<SurfaceStatusWire>,
    input_revisions: HashMap<String, u64>,
    status_revisions: HashMap<String, u64>,
    change_generation: u64,
}

#[derive(Clone)]
struct SessionSurfaceSnapshot {
    input: Option<SurfaceInputWire>,
    status: Option<SurfaceStatusWire>,
    change_generation: u64,
}

struct SurfacePublishOutcome {
    accepted_input_revision: Option<u64>,
    accepted_status_revision: Option<u64>,
}

struct SurfaceWatchState {
    registrations: Arc<Mutex<HashMap<SessionId, u64>>>,
    task: JoinHandle<()>,
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
struct OwnedTasks<T> {
    handles: Vec<JoinHandle<T>>,
    force_stop: Arc<AtomicBool>,
}

impl<T> OwnedTasks<T> {
    fn new(handles: Vec<JoinHandle<T>>, force_stop: Arc<AtomicBool>) -> Self {
        Self {
            handles,
            force_stop,
        }
    }

    async fn join_all(&mut self) -> Vec<Result<T, tokio::task::JoinError>> {
        let mut outcomes = Vec::with_capacity(self.handles.len());
        while let Some(handle) = self.handles.last_mut() {
            // Keep the handle inside `self` while it is awaited: cancelling
            // this join future must leave it visible to `Drop`, which raises
            // the fence and aborts every still-owned task.
            let outcome = handle.await;
            self.handles.pop();
            outcomes.push(outcome);
        }
        outcomes
    }
}

impl<T> Drop for OwnedTasks<T> {
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

#[derive(Clone, Copy)]
enum AppendCommitKind {
    General,
    Worker,
}

struct AppendCommitRequest {
    kind: AppendCommitKind,
    envelopes: Vec<RawEnvelope>,
    completed: oneshot::Sender<Result<Vec<RawEnvelope>, HaiderError>>,
}

enum AppendCommitMessage {
    Commit(AppendCommitRequest),
    Shutdown(oneshot::Sender<()>),
}

/// Profile-global group-commit admission shared by every session actor.
#[derive(Clone)]
struct AppendCommitter {
    requests: mpsc::UnboundedSender<AppendCommitMessage>,
}

impl AppendCommitter {
    async fn commit(
        &self,
        kind: AppendCommitKind,
        envelopes: Vec<RawEnvelope>,
    ) -> Result<Vec<RawEnvelope>, HaiderError> {
        let (completed, result) = oneshot::channel();
        self.requests
            .send(AppendCommitMessage::Commit(AppendCommitRequest {
                kind,
                envelopes,
                completed,
            }))
            .map_err(|_| hub_closed_store_error())?;
        result.await.map_err(|_| hub_closed_store_error())?
    }

    async fn shutdown(&self) {
        let (completed, result) = oneshot::channel();
        if self
            .requests
            .send(AppendCommitMessage::Shutdown(completed))
            .is_ok()
        {
            let _ = result.await;
        }
    }
}

async fn run_append_committer(
    store: SqliteStoreHandle,
    mut requests: mpsc::UnboundedReceiver<AppendCommitMessage>,
) {
    while let Some(message) = requests.recv().await {
        let first = match message {
            AppendCommitMessage::Commit(first) => first,
            AppendCommitMessage::Shutdown(completed) => {
                let _ = completed.send(());
                break;
            }
        };
        let mut pending = vec![first];
        let mut shutdown = None;
        while let Ok(message) = requests.try_recv() {
            match message {
                AppendCommitMessage::Commit(request) => pending.push(request),
                AppendCommitMessage::Shutdown(completed) => {
                    shutdown = Some(completed);
                    break;
                }
            }
        }

        let batches = pending
            .iter_mut()
            .map(|request| AppendGroupBatch {
                envelopes: std::mem::take(&mut request.envelopes),
                validate_worker_transitions: matches!(request.kind, AppendCommitKind::Worker),
            })
            .collect::<Vec<_>>();
        match store.append_group(batches).await {
            Ok(outcomes) => {
                for (request, outcome) in pending.into_iter().zip(outcomes) {
                    let _ = request.completed.send(outcome);
                }
            }
            Err(error) => {
                for request in pending {
                    let _ = request.completed.send(Err(error.clone()));
                }
            }
        }
        if let Some(completed) = shutdown {
            let _ = completed.send(());
            break;
        }
    }
}

/// Opaque same-process worker authority. Store generation fences restarts;
/// this token additionally fences a replaced supervisor in one generation.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkerLeaseId(String);

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecoveredMenuCoordinate {
    menu_id: MenuId,
    request_seq: u64,
    opening_generation: u64,
}

struct RegisteredWorker {
    lease_id: WorkerLeaseId,
    harness: Option<HarnessHandle>,
    /// Nonblocking notification only; the durable journal is the cancellation
    /// lag buffer. Replacement swaps this sender in actor order.
    cancellation_wake: Option<watch::Sender<u64>>,
}

/// Lease-fenced, session-scoped store surface handed to a worker (R1).
///
/// This type IS the append-exclusivity seal ([`SessionHub::append`] states
/// the law): a worker can reach committed reads, append-through-actor, CAS
/// artifacts, and the idle settle — nothing else. SQLite and cross-session
/// access are structurally unavailable, every append is identity-checked
/// against the lease's session and generation before it reaches the actor,
/// and the actor rejects the lease token itself once a successor supersedes
/// it (actor.rs charter). Cloning shares the one lease; it does not mint
/// authority.
#[derive(Clone)]
pub struct HubStoreHandle {
    hub: SessionHub,
    session_id: SessionId,
    worker_generation: u64,
    lease_id: WorkerLeaseId,
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
    envelope: Arc<RawEnvelope>,
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
        completed: oneshot::Sender<Result<Arc<[RawEnvelope]>, HaiderError>>,
    },
    CreateSession {
        command: SessionCreateCommand,
        completed: oneshot::Sender<Result<SessionCreateOutcome, HaiderError>>,
    },
    ForkSession {
        command: SessionForkCommand,
        completed: oneshot::Sender<Result<SessionForkOutcome, HaiderError>>,
    },
    CreateBranch {
        command: BranchCreateCommand,
        completed: oneshot::Sender<Result<BranchCreateOutcome, HaiderError>>,
    },
    SelectModel {
        command: SessionSelectModelCommand,
        completed: oneshot::Sender<Result<SessionSelectModelOutcome, HaiderError>>,
    },
    Rename {
        command: SessionRenameCommand,
        completed: oneshot::Sender<Result<SessionRenameOutcome, HaiderError>>,
    },
    Seen {
        command: SessionSeenCommand,
        completed: oneshot::Sender<Result<SessionSeenOutcome, HaiderError>>,
    },
    PinGraph {
        command: GraphPinCommand,
        expected_digest: Option<String>,
        completed: oneshot::Sender<Result<GraphPinOutcome, HaiderError>>,
    },
    AttachChildGraph {
        command: ChildGraphAttachCommand,
        completed: oneshot::Sender<Result<ChildGraphAttachOutcome, HaiderError>>,
    },
    ObserveChildTemplate {
        command: ChildTemplateObservationCommand,
        completed: oneshot::Sender<Result<ChildTemplateObservation, HaiderError>>,
    },
    OpenGraphRunSet {
        command: GraphRunSetOpenCommand,
        completed: oneshot::Sender<Result<GraphRunSetOpenOutcome, HaiderError>>,
    },
    SwitchGraph {
        command: GraphSwitchCommand,
        completed: oneshot::Sender<Result<GraphSwitchOutcome, HaiderError>>,
    },
    AbandonGraph {
        command: GraphAbandonCommand,
        completed: oneshot::Sender<Result<GraphAbandonOutcome, HaiderError>>,
    },
    RecordGraphEvidence {
        command: GraphEvidenceCommand,
        completed: oneshot::Sender<Result<GraphEvidenceOutcome, HaiderError>>,
    },
    RecordComputerEvidence {
        command: ComputerEvidenceCommand,
        completed: oneshot::Sender<Result<ComputerEvidenceOutcome, HaiderError>>,
    },
    GuardGraphFinalization {
        command: GraphFinalizationCommand,
        completed: oneshot::Sender<Result<GraphFinalizationOutcome, HaiderError>>,
    },
    RecordProcessSignal {
        command: ProcessSignalCommand,
        completed: oneshot::Sender<Result<ProcessSignalOutcome, HaiderError>>,
    },
    SelectEffort {
        command: SessionSelectEffortCommand,
        completed: oneshot::Sender<Result<SessionSelectEffortOutcome, HaiderError>>,
    },
    SelectAgentType {
        command: SessionSelectAgentTypeCommand,
        completed: oneshot::Sender<Result<SessionSelectAgentTypeOutcome, HaiderError>>,
    },
    SelectFast {
        command: SessionSelectFastCommand,
        completed: oneshot::Sender<Result<SessionSelectFastOutcome, HaiderError>>,
    },
    AcceptTurn {
        command: TurnAcceptCommand,
        completed: oneshot::Sender<Result<TurnAcceptOutcome, HaiderError>>,
    },
    AcceptRunRetry {
        command: RunRetryCommand,
        completed: oneshot::Sender<Result<RunRetryOutcome, HaiderError>>,
    },
    AcceptShellExec {
        command: ShellExecAcceptCommand,
        completed: oneshot::Sender<Result<ShellExecAcceptOutcome, HaiderError>>,
    },
    CancelTurn {
        command: TurnCancelCommand,
        completed: oneshot::Sender<Result<TurnCancelOutcome, HaiderError>>,
    },
    WorkerAppend {
        lease_id: WorkerLeaseId,
        expected_head: Option<u64>,
        envelopes: Vec<RawEnvelope>,
        completed: oneshot::Sender<Result<Arc<[RawEnvelope]>, HaiderError>>,
    },
    WorkerSettleIdle {
        lease_id: WorkerLeaseId,
        envelope: RawEnvelope,
        completed: oneshot::Sender<Result<Option<RawEnvelope>, HaiderError>>,
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
    AcquireWorkerLease {
        lease_id: WorkerLeaseId,
        cancellation_wake: Option<watch::Sender<u64>>,
        completed: oneshot::Sender<()>,
    },
    RegisterHarness {
        lease_id: WorkerLeaseId,
        harness: HarnessHandle,
        completed: oneshot::Sender<Result<(), HaiderError>>,
    },
    RegisterRecoveredHarness {
        lease_id: WorkerLeaseId,
        harness: HarnessHandle,
        menu: RecoveredMenuCoordinate,
        completed: oneshot::Sender<Result<(), HaiderError>>,
    },
    UnregisterHarness {
        lease_id: WorkerLeaseId,
    },
    StopIfQuiescent {
        completed: oneshot::Sender<Result<bool, HaiderError>>,
    },
    Stop,
}

/// One negotiated connection's authorization and attachment ownership.
pub struct HubConnection {
    hub: SessionHub,
    connection_id: String,
    capabilities: CapabilitySet,
    sink: Arc<dyn FrameSink>,
    /// Which transport carried this connection: raw-secret staging is
    /// LocalSameUid-only (R7), independent of the capability grant.
    transport: crate::accounts::ConnectionTransport,
    /// Connection-scoped staged secrets (R7): wiped on close/disconnect.
    stages: Mutex<crate::accounts::StagedSecrets>,
    /// At most one connection-scoped roster ticker. The task owns no
    /// connection clone, so aborting this handle tears the watch down
    /// immediately on close or drop.
    roster_watch: Mutex<Option<JoinHandle<()>>>,
    /// Account-registry change watcher (v0.0.938), torn down with the
    /// connection exactly like the roster watch.
    accounts_watch: Mutex<Option<JoinHandle<()>>>,
    /// One ticker serves a bounded set of per-session volatile watches.
    surface_watch: Mutex<Option<SurfaceWatchState>>,
    /// Write-free metafork reviews awaiting an explicit acceptance on this
    /// connection. Shared with command-capture facades; dropped on disconnect.
    metafork_reviews: Arc<Mutex<HashMap<String, String>>>,
    /// The transport-created identity lease. Response-capture facades share
    /// this lease, so they cannot independently own (and therefore cannot
    /// independently tear down) the caller's live identity.
    identity_lease: Arc<ConnectionIdentityLease>,
    closed: AtomicBool,
}

/// RAII ownership of an identity registered by `open_connection`.
///
/// Keeping teardown on the last shared lease, rather than on every
/// `HubConnection`-shaped view, makes borrowed command facades structurally
/// incapable of unregistering the identity they only use for authorization.
struct ConnectionIdentityLease {
    hub: SessionHub,
    connection_id: String,
}

impl Drop for ConnectionIdentityLease {
    fn drop(&mut self) {
        self.hub.clear_resident_binding(&self.connection_id);
        let Ok(attachments) = self
            .hub
            .detach_connection_registrations(&self.connection_id)
        else {
            return;
        };
        for (attachment_id, owner) in attachments {
            match tokio::runtime::Handle::try_current() {
                Ok(runtime) => {
                    runtime.spawn(async move {
                        SessionHub::finish_detach(&attachment_id, owner).await;
                    });
                }
                Err(_) => {
                    let _ = owner
                        .actor
                        .commands
                        .try_send(ActorCommand::Detach { attachment_id });
                }
            }
        }
    }
}

impl Drop for HubConnection {
    fn drop(&mut self) {
        if let Ok(watch) = self.roster_watch.get_mut()
            && let Some(task) = watch.take()
        {
            task.abort();
        }
        if let Ok(watch) = self.accounts_watch.get_mut()
            && let Some(task) = watch.take()
        {
            task.abort();
        }
        if let Ok(watch) = self.surface_watch.get_mut()
            && let Some(watch) = watch.take()
        {
            watch.task.abort();
        }
    }
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

/// Whether the hub delivered every committed drain suffix during its grace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionHubShutdownOutcome {
    Graceful,
    /// At least one final suffix could not be read or enqueued. The failure is
    /// recorded before this status is returned, and the daemon must report a
    /// forced shutdown rather than silently call the drain graceful.
    Forced,
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

    /// Ship-gate round 2: reconcile EVERY session's sidecar at boot — not
    /// just the ones startup recovery touched. The dirty state is
    /// memory-only, so a reconcile that failed in a PRIOR life left no
    /// durable retry obligation; the full sweep is the obligation. Cost is
    /// one tail read per current session (catch-up work only where a file
    /// is actually behind), before the endpoint binds.
    pub(crate) async fn reconcile_all_pipe_sidecars(
        &self,
        boot_journals: &HashMap<SessionId, Vec<RawEnvelope>>,
    ) {
        for (session_id, journal) in boot_journals {
            if let Err(error) = self
                .inner
                .pipe_native
                .maintain_from_boot_journal(&self.inner.store, session_id, journal)
                .await
            {
                tracing::warn!(
                    session_id = %session_id,
                    %error,
                    "boot native pipe sidecar reconciliation failed; journal remains authoritative"
                );
            }
        }
    }

    /// Reconciles journals committed by startup recovery before this hub
    /// existed. Sidecar projection is best-effort, just like post-commit actor
    /// maintenance: journal recovery remains authoritative if filesystem I/O
    /// fails.
    #[cfg(test)]
    pub(crate) async fn reconcile_pipe_sidecars(&self, session_ids: &[SessionId]) {
        for session_id in session_ids {
            if let Err(error) = self
                .inner
                .pipe_native
                .maintain(&self.inner.store, session_id, &[])
                .await
            {
                tracing::warn!(
                    session_id = %session_id,
                    %error,
                    "startup-recovered native pipe sidecar reconciliation failed; journal remains authoritative"
                );
            }
        }
    }

    /// Creates a hub with a semantic-boundary observer.
    pub fn with_observer(
        store: SqliteStoreHandle,
        config: SessionHubConfig,
        observer: Arc<dyn SessionHubObserver>,
    ) -> Result<Self, SessionHubError> {
        config.validate().map_err(SessionHubError::InvalidConfig)?;
        let cache_diagnostic_key =
            load_or_create_cache_diagnostic_key(store.root()).map_err(|error| {
                SessionHubError::Task(format!("cannot load cache diagnostic key: {error}"))
            })?;
        let device_id = DeviceId::new(format!("daemon-session-hub-{}", store.worker_generation()));
        let pipe_native = Arc::new(crate::pipe_native::PipeNativeWriter::new(store.root()));
        let (append_requests, append_receiver) = mpsc::unbounded_channel();
        let append_committer = AppendCommitter {
            requests: append_requests,
        };
        let append_commit_task = tokio::spawn(run_append_committer(store.clone(), append_receiver));
        let (surface_publications, _) = watch::channel(0_u64);
        let (roster_publications, _) = broadcast::channel(1_024);
        let (haider_code_plan_changes, _) = watch::channel(0_u64);
        let hooks = Arc::new(Mutex::new(None));
        let observe_digests = Arc::new(rpc::ObserveDigestCache::default());
        let commit_projection = Arc::new(CommitProjection {
            hooks: Arc::clone(&hooks),
            observe_digests: Arc::clone(&observe_digests),
            roster_publications: roster_publications.clone(),
            haider_code_plan_changes: haider_code_plan_changes.clone(),
        });
        let inner = Arc::new(HubInner {
            store,
            append_committer,
            append_commit_task: Mutex::new(Some(append_commit_task)),
            pipe_native,
            config,
            observer,
            metrics: Arc::new(HubMetrics::default()),
            actors: Mutex::new(HashMap::new()),
            diagnostic_sinks: Mutex::new(HashMap::new()),
            resident_binding: Mutex::new(ResidentBindingRegistry::default()),
            resident_binding_viewers: Mutex::new(HashSet::new()),
            surfaces: Mutex::new(HashMap::new()),
            surface_publications,
            deleting_sessions: Mutex::new(HashSet::new()),
            actor_tasks: Mutex::new(Vec::new()),
            replay_tasks: Mutex::new(Vec::new()),
            attachments: Mutex::new(HashMap::new()),
            attachment_slots: Mutex::new(AttachmentSlots::default()),
            draining: AtomicBool::new(false),
            force_stop: Arc::new(AtomicBool::new(false)),
            device_id,
            cache_diagnostic_key,
            worker_manager: Mutex::new(None),
            accounts: Mutex::new(None),
            creatable_providers: Mutex::new(None),
            hooks,
            commit_projection,
            observe_digests,
            roster_publications,
            haider_code_plan_changes,
            usage_report: Mutex::new(None),
            tasks: crate::tasks::TaskRegistry::default(),
            web_degrade: Mutex::new(HashMap::new()),
            prompt_history: PromptHistoryCache::default(),
        });
        let hub = Self { inner };
        hub.spawn_profile_fault_watcher();
        Ok(hub)
    }

    fn spawn_profile_fault_watcher(&self) {
        let mut faults = self.inner.store.subscribe_profile_fault();
        let weak = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            while faults.changed().await.is_ok() {
                let Some(inner) = weak.upgrade() else {
                    break;
                };
                let (frame, fault_latched) = {
                    let fault = faults.borrow_and_update();
                    let frame = if let Some(fault) = fault.as_ref() {
                        profile_store_fault_frame(fault)
                    } else {
                        WireFrame::ProtocolError(ProtocolError {
                            code: "store_healthy".into(),
                            message: "profile store is writable again".into(),
                            fatal: false,
                            presentation: None,
                            failed_write_ids: Vec::new(),
                        })
                    };
                    (frame, fault.is_some())
                };
                let sinks = inner
                    .diagnostic_sinks
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .values()
                    .cloned()
                    .collect::<Vec<_>>();
                if let Some((last, sinks)) = sinks.split_last() {
                    for sink in sinks {
                        let _ = sink.try_send(frame.clone());
                    }
                    let _ = last.try_send(frame);
                }
                if !fault_latched {
                    continue;
                }
                // A bounded health probe keeps the banner latched until the
                // filesystem actually accepts a write. It does not replay
                // failed mutations; their ids remain explicitly reported.
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                let _ = inner.store.probe_writable().await;
            }
        });
    }

    pub(crate) fn task_registry(&self) -> &crate::tasks::TaskRegistry {
        &self.inner.tasks
    }

    /// W-B: this session's web-capability degrade snapshot (Default = no
    /// degrade). Poison-tolerant like the task registry.
    pub(crate) fn web_degrade(
        &self,
        session_id: &SessionId,
    ) -> crate::worker::WebCapabilityDegrade {
        self.inner
            .web_degrade
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session_id)
            .copied()
            .unwrap_or_default()
    }

    /// W-B: latches "anthropic server web tools 400ed" for this session —
    /// the next turn declares no server tools and falls back to the local
    /// `web_fetch` client tool.
    pub(crate) fn degrade_anthropic_web_tools(&self, session_id: &SessionId) {
        self.inner
            .web_degrade
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(session_id.clone())
            .or_default()
            .anthropic_web_tools = true;
    }

    /// W-B: latches "codex alpha/search is gone (404/410)" for this session —
    /// the client `web_search` tool stops advertising (no retry storm).
    pub(crate) fn degrade_openai_alpha_search(&self, session_id: &SessionId) {
        self.inner
            .web_degrade
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(session_id.clone())
            .or_default()
            .openai_alpha_search = true;
    }

    /// Stores one bounded background-task output payload in the profile CAS.
    pub(crate) async fn put_internal_artifact(
        &self,
        bytes: Vec<u8>,
    ) -> Result<haider_protocol::ids::ArtifactRef, HaiderError> {
        self.inner.store.put(bytes).await
    }

    /// Direct durable read used only by the delete fence, where the session
    /// actor is already stopped and `read_internal_session` cannot route.
    pub(crate) async fn store_read(
        &self,
        session_id: &SessionId,
        since_seq: u64,
        limit: usize,
    ) -> Result<Vec<RawEnvelope>, HaiderError> {
        haider_core::StoreHandle::read(&self.inner.store, session_id, since_seq, limit).await
    }

    pub(crate) fn install_worker_manager(
        &self,
        manager: WorkerManagerHandle,
    ) -> Result<(), SessionHubError> {
        let mut installed = lock(&self.inner.worker_manager)?;
        if installed.is_some() {
            return Err(SessionHubError::Task(
                "worker manager is already installed".into(),
            ));
        }
        *installed = Some(manager);
        Ok(())
    }

    pub(crate) fn install_hooks(
        &self,
        hooks: crate::hooks::HookService,
    ) -> Result<(), SessionHubError> {
        let mut installed = lock(&self.inner.hooks)?;
        if installed.is_some() {
            return Err(SessionHubError::Task(
                "hook service is already installed".into(),
            ));
        }
        *installed = Some(hooks.downgrade());
        Ok(())
    }

    pub(crate) fn hooks(&self) -> Result<Option<crate::hooks::HookService>, SessionHubError> {
        Ok(lock(&self.inner.hooks)?
            .as_ref()
            .and_then(crate::hooks::WeakHookService::upgrade))
    }

    /// Installs the account facade (actor route + descriptor snapshot),
    /// mirroring the worker-manager installation seam.
    pub(crate) fn install_accounts(
        &self,
        facade: crate::accounts::AccountsFacade,
    ) -> Result<(), SessionHubError> {
        let mut installed = lock(&self.inner.accounts)?;
        if installed.is_some() {
            return Err(SessionHubError::Task(
                "account facade is already installed".into(),
            ));
        }
        *installed = Some(facade);
        Ok(())
    }

    pub(crate) fn accounts(
        &self,
    ) -> Result<Option<crate::accounts::AccountsFacade>, SessionHubError> {
        Ok(lock(&self.inner.accounts)?.clone())
    }

    /// Installs the `usage.report` service (U1), mirroring the account
    /// facade installation seam.
    pub(crate) fn install_usage_report(
        &self,
        service: Arc<crate::usage_report::UsageReportService>,
    ) -> Result<(), SessionHubError> {
        let mut installed = lock(&self.inner.usage_report)?;
        if installed.is_some() {
            return Err(SessionHubError::Task(
                "usage-report service is already installed".into(),
            ));
        }
        *installed = Some(service);
        Ok(())
    }

    pub(crate) fn usage_report_service(
        &self,
    ) -> Result<Option<Arc<crate::usage_report::UsageReportService>>, SessionHubError> {
        Ok(lock(&self.inner.usage_report)?.clone())
    }

    pub(crate) fn subscribe_haider_code_plan_changes(&self) -> watch::Receiver<u64> {
        self.inner.haider_code_plan_changes.subscribe()
    }

    fn notify_haider_code_plan_change(&self) {
        self.inner
            .haider_code_plan_changes
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }

    pub(crate) fn attached_session_connections(
        &self,
    ) -> Result<Vec<(String, SessionId)>, SessionHubError> {
        Ok(lock(&self.inner.attachments)?
            .values()
            .map(|owner| (owner.connection_id.clone(), owner.session_id.clone()))
            .collect())
    }

    pub(crate) fn publish_haider_code_plan_status(
        &self,
        connection_ids: &[String],
        frame: WireFrame,
    ) {
        let recipients = connection_ids
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        let Ok(sinks) = self.inner.diagnostic_sinks.lock() else {
            return;
        };
        for (connection_id, sink) in sinks.iter() {
            if recipients.contains(connection_id.as_str()) {
                let _ = sink.try_send_droppable(frame.clone());
            }
        }
    }

    /// Installs the ONE `session.create` provider whitelist (D3-5): the
    /// dependency configuration answers "creatable providers"; nothing else
    /// may.
    pub(crate) fn install_creatable_providers(
        &self,
        providers: std::collections::BTreeSet<String>,
    ) -> Result<(), SessionHubError> {
        let mut installed = lock(&self.inner.creatable_providers)?;
        if installed.is_some() {
            return Err(SessionHubError::Task(
                "creatable-provider registry is already installed".into(),
            ));
        }
        *installed = Some(providers);
        Ok(())
    }

    pub(crate) fn creatable_providers(
        &self,
    ) -> Result<Option<std::collections::BTreeSet<String>>, SessionHubError> {
        Ok(lock(&self.inner.creatable_providers)?.clone())
    }

    /// Snapshot of monotonic lag-buffer and delivery-pressure counters.
    pub fn metrics(&self) -> SessionHubMetrics {
        self.inner.metrics.snapshot()
    }

    fn worker_manager(&self) -> Result<WorkerManagerHandle, SessionHubError> {
        lock(&self.inner.worker_manager)?
            .clone()
            .ok_or_else(|| SessionHubError::Task("worker manager is not installed".into()))
    }

    pub(crate) async fn session_metadata(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<haider_protocol::session::SessionMetadataV1>, HaiderError> {
        self.inner.store.session_metadata(session_id).await
    }

    /// C1 — one registered Loom workflow (worker's typed-node tail).
    pub(crate) async fn loom_workflow(
        &self,
        id: &str,
    ) -> Result<Option<haider_protocol::loom::LoomWorkflow>, HaiderError> {
        self.inner.store.loom_workflow(id.to_owned()).await
    }

    /// C2 — one registered Loom agent type (typed spawns).
    pub(crate) async fn loom_agent_type(
        &self,
        id: &str,
    ) -> Result<Option<haider_protocol::loom::LoomAgentType>, HaiderError> {
        self.inner.store.loom_agent_type(id.to_owned()).await
    }

    /// E1 — the whole registry in one read (the volatile-tail inventory).
    pub(crate) async fn loom_registry(
        &self,
    ) -> Result<
        (
            Vec<haider_protocol::loom::LoomAgentType>,
            Vec<haider_protocol::loom::LoomWorkflow>,
        ),
        HaiderError,
    > {
        Ok((
            self.inner.store.loom_agent_types().await?,
            self.inner.store.loom_workflows().await?,
        ))
    }

    /// E2 — plan-gated registration from the session agent's tool path.
    pub(crate) async fn loom_register_workflow(
        &self,
        source: String,
    ) -> Result<haider_protocol::loom::LoomRegistration, HaiderError> {
        self.inner.store.loom_register_workflow(source).await
    }

    pub(crate) async fn loom_register_agent_type(
        &self,
        record: haider_protocol::loom::LoomAgentType,
    ) -> Result<haider_protocol::loom::LoomRegistration, HaiderError> {
        self.inner.store.loom_register_agent_type(record).await
    }

    /// Narrow daemon-internal session creation used by local delegation.
    /// It preserves the same unfenced receipt preflight and actor-routed
    /// transaction as the wire method without fabricating an RPC connection.
    pub(crate) async fn create_internal_session(
        &self,
        command: SessionCreateCommand,
    ) -> Result<CreatedSession, HaiderError> {
        if let Some(created) = self
            .inner
            .store
            .session_create_receipt(
                command.command_id.clone(),
                command.request_digest.clone(),
                command.request_json.clone(),
            )
            .await?
        {
            return Ok(created);
        }
        match self
            .create_session(command)
            .await
            .map_err(hub_error_as_store)?
        {
            SessionCreateOutcome::Committed { created, .. }
            | SessionCreateOutcome::IdempotentReplay { created } => Ok(created),
        }
    }

    /// Narrow daemon-internal turn acceptance used by local delegation.
    pub(crate) async fn accept_internal_turn(
        &self,
        command: TurnAcceptCommand,
    ) -> Result<AcceptedTurn, HaiderError> {
        if let Some(accepted) = self
            .inner
            .store
            .turn_accept_receipt(
                command.command_id.clone(),
                command.request_digest.clone(),
                command.request_json.clone(),
            )
            .await?
        {
            return Ok(accepted);
        }
        match self
            .accept_turn(command)
            .await
            .map_err(hub_error_as_store)?
        {
            TurnAcceptOutcome::Committed { accepted, .. }
            | TurnAcceptOutcome::IdempotentReplay { accepted } => Ok(accepted),
        }
    }

    pub(crate) async fn submit_internal_turn(
        &self,
        accepted: AcceptedTurn,
    ) -> Result<(), HaiderError> {
        self.worker_manager()
            .map_err(hub_error_as_store)?
            .submit(accepted)
            .await
    }

    pub(crate) async fn submit_internal_nudge(
        &self,
        accepted: AcceptedTurn,
        text: String,
    ) -> Result<(), HaiderError> {
        self.worker_manager()
            .map_err(hub_error_as_store)?
            .nudge(
                accepted.session_id,
                accepted.run_id,
                accepted.accepted_seq,
                text,
            )
            .await
    }

    pub(crate) async fn submit_internal_subturn(
        &self,
        accepted: AcceptedTurn,
        text: String,
    ) -> Result<(), HaiderError> {
        self.worker_manager()
            .map_err(hub_error_as_store)?
            .subturn(
                accepted.session_id,
                accepted.run_id,
                accepted.accepted_seq,
                text,
            )
            .await
    }

    /// Receipt-backed daemon-internal cancellation used by delegation
    /// supervision and subtree sweeps.
    pub(crate) async fn cancel_internal_turn(
        &self,
        command: TurnCancelCommand,
    ) -> Result<CancelledTurn, HaiderError> {
        if let Some(cancelled) = self
            .inner
            .store
            .turn_cancel_receipt(
                command.command_id.clone(),
                command.request_digest.clone(),
                command.request_json.clone(),
            )
            .await?
        {
            return Ok(cancelled);
        }
        match self
            .cancel_turn(command)
            .await
            .map_err(hub_error_as_store)?
        {
            TurnCancelOutcome::Committed { cancelled, .. }
            | TurnCancelOutcome::IdempotentReplay { cancelled } => Ok(cancelled),
        }
    }

    pub(crate) async fn has_internal_cancel_receipt(
        &self,
        command_id: String,
        request_digest: String,
        request_json: String,
    ) -> Result<bool, HaiderError> {
        self.inner
            .store
            .turn_cancel_receipt(command_id, request_digest, request_json)
            .await
            .map(|receipt| receipt.is_some())
    }

    pub(crate) fn worker_generation(&self) -> u64 {
        self.inner.store.worker_generation()
    }

    pub(crate) fn device_id(&self) -> DeviceId {
        self.inner.device_id.clone()
    }

    pub(crate) fn cache_diagnostic_key(&self) -> CacheDiagnosticKey {
        self.inner.cache_diagnostic_key.clone()
    }

    pub(crate) async fn create_delegation(
        &self,
        record: haider_core::DelegationRecord,
    ) -> Result<haider_core::DelegationCreateOutcome, HaiderError> {
        self.inner.store.create_delegation(record).await
    }

    pub(crate) fn notify_roster_session(&self, session_id: SessionId) {
        let _ = self.inner.roster_publications.send(session_id);
    }

    pub(crate) async fn delegation(
        &self,
        agent: haider_protocol::ids::AgentId,
    ) -> Result<Option<haider_core::DelegationRecord>, HaiderError> {
        self.inner.store.delegation(agent).await
    }

    pub(crate) async fn delegation_for_child_session(
        &self,
        session_id: SessionId,
    ) -> Result<Option<haider_core::DelegationRecord>, HaiderError> {
        self.inner
            .store
            .delegation_for_child_session(session_id)
            .await
    }

    pub(crate) async fn delegations_for_parent_run(
        &self,
        session_id: SessionId,
        run_id: RunId,
    ) -> Result<Vec<haider_core::DelegationRecord>, HaiderError> {
        self.inner
            .store
            .delegations_for_parent_run(session_id, run_id)
            .await
    }

    pub(crate) async fn delegation_descendants(
        &self,
        session_id: SessionId,
        max_nodes: usize,
        max_depth: u32,
    ) -> Result<haider_core::DelegationDescendants, HaiderError> {
        self.inner
            .store
            .delegation_descendants(session_id, max_nodes, max_depth)
            .await
    }

    pub(crate) async fn mark_delegation_running(
        &self,
        agent: haider_protocol::ids::AgentId,
    ) -> Result<haider_core::DelegationRecord, HaiderError> {
        self.inner.store.mark_delegation_running(agent).await
    }

    pub(crate) async fn record_delegation_report(
        &self,
        agent: haider_protocol::ids::AgentId,
        report: haider_protocol::agent::ChildReport,
    ) -> Result<haider_core::DelegationRecord, HaiderError> {
        self.inner
            .store
            .record_delegation_report(agent, report)
            .await
    }

    pub(crate) async fn mark_delegation_collected(
        &self,
        agent: haider_protocol::ids::AgentId,
    ) -> Result<haider_core::DelegationRecord, HaiderError> {
        self.inner.store.mark_delegation_collected(agent).await
    }

    pub(crate) async fn read_internal_session(
        &self,
        session_id: &SessionId,
        since_seq: u64,
        limit: usize,
    ) -> Result<Vec<RawEnvelope>, HaiderError> {
        self.inner.store.read(session_id, since_seq, limit).await
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
        transport: crate::accounts::ConnectionTransport,
    ) -> Result<HubConnection, SessionHubError> {
        if self.inner.draining.load(Ordering::Acquire) {
            return Err(SessionHubError::Closed);
        }
        let connection_id = random_id("connection")?;
        let may_view_binding =
            capabilities.contains(&Capability::View) || capabilities.contains(&Capability::Control);
        // The state lock spans registration and baseline admission. A
        // concurrent publisher therefore queues either before this baseline
        // is sampled or after it is admitted, never between the two and in
        // reverse order.
        let resident_binding = lock(&self.inner.resident_binding)?;
        let mut resident_binding_viewers = lock(&self.inner.resident_binding_viewers)?;
        let mut diagnostic_sinks = lock(&self.inner.diagnostic_sinks)?;
        diagnostic_sinks.insert(connection_id.clone(), Arc::clone(&sink));
        if may_view_binding {
            resident_binding_viewers.insert(connection_id.clone());
        }
        if may_view_binding {
            let (session_id, worker_generation, binding_token) =
                resident_binding.visible().unwrap_or_else(|| {
                    // With no publisher and no recorded vacancy, `None` is still
                    // authoritative state for this daemon generation. Using the
                    // current store generation (rather than 0) lets consumers
                    // apply the explicit unbound baseline through the same stale-
                    // generation fence as later binding announcements. This is
                    // synthesized per open, not retained as a publisher event.
                    (None, self.inner.store.worker_generation(), None)
                });
            if sink
                .try_send(WireFrame::ResidentSessionBinding {
                    session_id,
                    worker_generation,
                    binding_token,
                })
                .is_err()
            {
                diagnostic_sinks.remove(&connection_id);
                resident_binding_viewers.remove(&connection_id);
                sink.close_after_required_delivery_failure();
                return Err(SessionHubError::Delivery);
            }
        }
        drop(diagnostic_sinks);
        drop(resident_binding_viewers);
        drop(resident_binding);
        if let Some(fault) = self.inner.store.profile_fault() {
            let _ = sink.try_send(profile_store_fault_frame(&fault));
        }
        let identity_lease = Arc::new(ConnectionIdentityLease {
            hub: self.clone(),
            connection_id: connection_id.clone(),
        });
        Ok(HubConnection {
            hub: self.clone(),
            connection_id,
            capabilities,
            sink,
            transport,
            stages: Mutex::new(crate::accounts::StagedSecrets::default()),
            roster_watch: Mutex::new(None),
            accounts_watch: Mutex::new(None),
            surface_watch: Mutex::new(None),
            metafork_reviews: Arc::new(Mutex::new(HashMap::new())),
            identity_lease,
            closed: AtomicBool::new(false),
        })
    }

    /// Routes an append through the owning session actor.
    ///
    /// Every live-daemon append must pass a session actor: INVARIANTs 1 and 2
    /// (module doc) are properties of the actor's command order, so an append
    /// that bypassed the actor could publish around a registration.
    ///
    /// APPEND EXCLUSIVITY — structural since W3c1: every live worker holds
    /// only a lease-fenced [`HubStoreHandle`] (pinned by
    /// `worker_surface_is_structurally_lease_scoped`), so the W3b2
    /// "discipline, not shape" caveat now applies only to the paths that run
    /// while NO hub exists: startup recovery (`turn_recovery.rs`,
    /// `haider_core::recovery`), the standalone CLI, and test seeding. Those
    /// remain discipline-held. This facade method is actor-routed test and
    /// non-worker publication; production workers append through their lease.
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

    /// Routes atomic session creation through the candidate session actor.
    /// A guessed concurrent attachment is therefore ordered with the
    /// `Created` commit/publication by the same INV-1/INV-2 actor step.
    async fn create_session(
        &self,
        command: SessionCreateCommand,
    ) -> Result<SessionCreateOutcome, SessionHubError> {
        let actor = self.actor_for(command.session_id.clone()).await?;
        let (completed, result) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::CreateSession { command, completed })
            .await
            .map_err(|_| SessionHubError::Closed)?;
        result
            .await
            .map_err(|_| SessionHubError::Closed)?
            .map_err(Into::into)
    }

    /// Preflights a durable receipt before workspace I/O. A same-command
    /// retry must recover its committed response even if that workspace path
    /// disappeared after the original commit.
    async fn session_create_receipt(
        &self,
        command_id: &CommandId,
        request_digest: &str,
        request_json: &str,
    ) -> Result<Option<CreatedSession>, SessionHubError> {
        self.inner
            .store
            .session_create_receipt(
                command_id.0.clone(),
                request_digest.to_owned(),
                request_json.to_owned(),
            )
            .await
            .map_err(Into::into)
    }

    async fn session_fork_receipt(
        &self,
        command_id: &CommandId,
        request_digest: &str,
        request_json: &str,
        metafork: bool,
    ) -> Result<Option<CreatedSessionFork>, SessionHubError> {
        let command_id = command_id.0.clone();
        let request_digest = request_digest.to_owned();
        let request_json = request_json.to_owned();
        if metafork {
            self.inner
                .store
                .session_metafork_receipt(command_id, request_digest, request_json)
                .await
                .map_err(Into::into)
        } else {
            self.inner
                .store
                .session_fork_receipt(command_id, request_digest, request_json)
                .await
                .map_err(Into::into)
        }
    }

    /// Routes creation through the candidate child actor so attachment,
    /// sidecar, and roster publication are ordered after the atomic clone.
    async fn fork_session(
        &self,
        command: SessionForkCommand,
    ) -> Result<SessionForkOutcome, SessionHubError> {
        let candidate_session_id = command.session_id.clone();
        let actor = self.actor_for(candidate_session_id.clone()).await?;
        let (completed, result) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::ForkSession { command, completed })
            .await
            .map_err(|_| SessionHubError::Closed)?;
        let outcome = result
            .await
            .map_err(|_| SessionHubError::Closed)?
            .map_err(SessionHubError::from)?;
        if matches!(
            &outcome,
            SessionForkOutcome::IdempotentReplay { created }
                if created.session_id != candidate_session_id
        ) {
            self.stop_discarded_candidate_actor(&candidate_session_id, &actor)
                .await?;
        }
        Ok(outcome)
    }

    async fn stop_discarded_candidate_actor(
        &self,
        candidate_session_id: &SessionId,
        candidate_actor: &SessionActorHandle,
    ) -> Result<(), SessionHubError> {
        let removed = {
            let mut actors = lock(&self.inner.actors)?;
            let owns_candidate = actors
                .get(candidate_session_id)
                .is_some_and(|current| current.commands.same_channel(&candidate_actor.commands));
            owns_candidate
                .then(|| actors.remove(candidate_session_id))
                .flatten()
        };
        if let Some(actor) = removed {
            let _ = actor.commands.send(ActorCommand::Stop).await;
        }
        Ok(())
    }

    async fn branch_create_receipt(
        &self,
        command_id: &CommandId,
        request_digest: &str,
        request_json: &str,
    ) -> Result<Option<CreatedBranch>, SessionHubError> {
        self.inner
            .store
            .branch_create_receipt(
                command_id.0.clone(),
                request_digest.to_owned(),
                request_json.to_owned(),
            )
            .await
            .map_err(Into::into)
    }

    async fn create_branch(
        &self,
        command: BranchCreateCommand,
    ) -> Result<BranchCreateOutcome, SessionHubError> {
        let actor = self.actor_for(command.session_id.clone()).await?;
        let (completed, result) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::CreateBranch { command, completed })
            .await
            .map_err(|_| SessionHubError::Closed)?;
        result
            .await
            .map_err(|_| SessionHubError::Closed)?
            .map_err(Into::into)
    }

    async fn command_receipt_worker_generation(
        &self,
        command_id: &CommandId,
        expected_method: &str,
    ) -> Result<Option<u64>, SessionHubError> {
        self.inner
            .store
            .command_receipt_worker_generation(command_id.0.clone(), expected_method.to_owned())
            .await
            .map_err(Into::into)
    }

    async fn session_select_model_receipt(
        &self,
        command_id: &CommandId,
        request_digest: &str,
        request_json: &str,
    ) -> Result<Option<SelectedModel>, SessionHubError> {
        self.inner
            .store
            .session_select_model_receipt(
                command_id.0.clone(),
                request_digest.to_owned(),
                request_json.to_owned(),
            )
            .await
            .map_err(Into::into)
    }

    /// `pub(crate)` for the in-crate runtime laws (the internal-test
    /// convention of `accept_internal_turn`): a committed selection must go
    /// THROUGH the actor arm so the actor's in-memory head advances with the
    /// fact — a store-direct select desyncs the compaction head CAS.
    pub(crate) async fn select_session_model(
        &self,
        command: SessionSelectModelCommand,
    ) -> Result<SessionSelectModelOutcome, SessionHubError> {
        let actor = self.actor_for(command.session_id.clone()).await?;
        let (completed, result) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::SelectModel { command, completed })
            .await
            .map_err(|_| SessionHubError::Closed)?;
        result
            .await
            .map_err(|_| SessionHubError::Closed)?
            .map_err(Into::into)
    }

    async fn session_rename_receipt(
        &self,
        command_id: &CommandId,
        request_digest: &str,
        request_json: &str,
    ) -> Result<Option<RenamedSession>, SessionHubError> {
        self.inner
            .store
            .session_rename_receipt(
                command_id.0.clone(),
                request_digest.to_owned(),
                request_json.to_owned(),
            )
            .await
            .map_err(Into::into)
    }

    /// G2: a committed rename goes THROUGH the actor arm for the same
    /// reason `select_session_model` does — the actor's in-memory head must
    /// advance with the published `session_renamed` fact or the compaction
    /// head CAS desyncs. `pub(crate)` because the daemon's internal
    /// auto-title (first accept) issues the same command.
    pub(crate) async fn rename_session(
        &self,
        command: SessionRenameCommand,
    ) -> Result<SessionRenameOutcome, SessionHubError> {
        let actor = self.actor_for(command.session_id.clone()).await?;
        let (completed, result) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::Rename { command, completed })
            .await
            .map_err(|_| SessionHubError::Closed)?;
        result
            .await
            .map_err(|_| SessionHubError::Closed)?
            .map_err(Into::into)
    }

    async fn session_seen_receipt(
        &self,
        command_id: &CommandId,
        request_digest: &str,
        request_json: &str,
    ) -> Result<Option<SeenSession>, SessionHubError> {
        self.inner
            .store
            .session_seen_receipt(
                command_id.0.clone(),
                request_digest.to_owned(),
                request_json.to_owned(),
            )
            .await
            .map_err(Into::into)
    }

    /// An attention acknowledgement shares the serialized actor with every
    /// session write: the `session_seen` fact advances the actor head and
    /// wakes the common roster projection after its durable transaction.
    pub(crate) async fn mark_session_seen(
        &self,
        command: SessionSeenCommand,
    ) -> Result<SessionSeenOutcome, SessionHubError> {
        let actor = self.actor_for(command.session_id.clone()).await?;
        let (completed, result) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::Seen { command, completed })
            .await
            .map_err(|_| SessionHubError::Closed)?;
        result
            .await
            .map_err(|_| SessionHubError::Closed)?
            .map_err(Into::into)
    }

    pub(crate) async fn graph_status(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<haider_protocol::graph::GraphStatus>, SessionHubError> {
        self.inner
            .store
            .graph_status(session_id)
            .await
            .map_err(Into::into)
    }

    pub(crate) async fn graph_inspect(
        &self,
        session_id: &SessionId,
        cursor: Option<String>,
        limit: u32,
    ) -> Result<GraphInspectResult, SessionHubError> {
        self.inner
            .store
            .graph_inspect(session_id, cursor, limit)
            .await
            .map_err(Into::into)
    }

    pub(crate) async fn guard_graph_finalization(
        &self,
        command: GraphFinalizationCommand,
    ) -> Result<GraphFinalizationOutcome, SessionHubError> {
        let actor = self.actor_for(command.session_id.clone()).await?;
        let (completed, result) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::GuardGraphFinalization { command, completed })
            .await
            .map_err(|_| SessionHubError::Closed)?;
        result
            .await
            .map_err(|_| SessionHubError::Closed)?
            .map_err(Into::into)
    }

    async fn graph_pin_receipt(
        &self,
        command_id: &CommandId,
        request_digest: &str,
        request_json: &str,
    ) -> Result<Option<PinnedGraph>, SessionHubError> {
        self.inner
            .store
            .graph_pin_receipt(
                command_id.0.clone(),
                request_digest.to_owned(),
                request_json.to_owned(),
            )
            .await
            .map_err(Into::into)
    }

    pub(crate) async fn pin_graph(
        &self,
        command: GraphPinCommand,
    ) -> Result<GraphPinOutcome, SessionHubError> {
        let actor = self.actor_for(command.session_id.clone()).await?;
        let (completed, result) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::PinGraph {
                command,
                expected_digest: None,
                completed,
            })
            .await
            .map_err(|_| SessionHubError::Closed)?;
        result
            .await
            .map_err(|_| SessionHubError::Closed)?
            .map_err(Into::into)
    }

    pub(crate) async fn pin_graph_matching_digest(
        &self,
        command: GraphPinCommand,
        expected_digest: String,
    ) -> Result<GraphPinOutcome, SessionHubError> {
        let actor = self.actor_for(command.session_id.clone()).await?;
        let (completed, result) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::PinGraph {
                command,
                expected_digest: Some(expected_digest),
                completed,
            })
            .await
            .map_err(|_| SessionHubError::Closed)?;
        result
            .await
            .map_err(|_| SessionHubError::Closed)?
            .map_err(Into::into)
    }

    pub(crate) async fn attach_child_graph(
        &self,
        command: ChildGraphAttachCommand,
    ) -> Result<ChildGraphAttachOutcome, SessionHubError> {
        let actor = self.actor_for(command.session_id.clone()).await?;
        let (completed, result) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::AttachChildGraph { command, completed })
            .await
            .map_err(|_| SessionHubError::Closed)?;
        result
            .await
            .map_err(|_| SessionHubError::Closed)?
            .map_err(Into::into)
    }

    async fn graph_run_set_open_receipt(
        &self,
        command_id: &CommandId,
        request_digest: &str,
        request_json: &str,
    ) -> Result<Option<OpenedGraphRunSet>, SessionHubError> {
        self.inner
            .store
            .graph_run_set_open_receipt(
                command_id.0.clone(),
                request_digest.to_owned(),
                request_json.to_owned(),
            )
            .await
            .map_err(Into::into)
    }

    pub(crate) async fn open_graph_run_set(
        &self,
        command: GraphRunSetOpenCommand,
    ) -> Result<GraphRunSetOpenOutcome, SessionHubError> {
        let actor = self.actor_for(command.session_id.clone()).await?;
        let (completed, result) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::OpenGraphRunSet { command, completed })
            .await
            .map_err(|_| SessionHubError::Closed)?;
        result
            .await
            .map_err(|_| SessionHubError::Closed)?
            .map_err(Into::into)
    }

    async fn graph_switch_receipt(
        &self,
        command_id: &CommandId,
        request_digest: &str,
        request_json: &str,
    ) -> Result<Option<SwitchedGraph>, SessionHubError> {
        self.inner
            .store
            .graph_switch_receipt(
                command_id.0.clone(),
                request_digest.to_owned(),
                request_json.to_owned(),
            )
            .await
            .map_err(Into::into)
    }

    pub(crate) async fn switch_graph(
        &self,
        command: GraphSwitchCommand,
    ) -> Result<GraphSwitchOutcome, SessionHubError> {
        let actor = self.actor_for(command.session_id.clone()).await?;
        let (completed, result) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::SwitchGraph { command, completed })
            .await
            .map_err(|_| SessionHubError::Closed)?;
        result
            .await
            .map_err(|_| SessionHubError::Closed)?
            .map_err(Into::into)
    }

    async fn graph_abandon_receipt(
        &self,
        command_id: &CommandId,
        request_digest: &str,
        request_json: &str,
    ) -> Result<Option<AbandonedGraph>, SessionHubError> {
        self.inner
            .store
            .graph_abandon_receipt(
                command_id.0.clone(),
                request_digest.to_owned(),
                request_json.to_owned(),
            )
            .await
            .map_err(Into::into)
    }

    pub(crate) async fn abandon_graph(
        &self,
        command: GraphAbandonCommand,
    ) -> Result<GraphAbandonOutcome, SessionHubError> {
        let actor = self.actor_for(command.session_id.clone()).await?;
        let (completed, result) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::AbandonGraph { command, completed })
            .await
            .map_err(|_| SessionHubError::Closed)?;
        result
            .await
            .map_err(|_| SessionHubError::Closed)?
            .map_err(Into::into)
    }

    pub(crate) async fn record_graph_evidence(
        &self,
        command: GraphEvidenceCommand,
    ) -> Result<GraphEvidenceOutcome, SessionHubError> {
        let actor = self.actor_for(command.session_id.clone()).await?;
        let (completed, result) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::RecordGraphEvidence { command, completed })
            .await
            .map_err(|_| SessionHubError::Closed)?;
        result
            .await
            .map_err(|_| SessionHubError::Closed)?
            .map_err(Into::into)
    }

    pub(crate) async fn record_computer_evidence(
        &self,
        command: ComputerEvidenceCommand,
    ) -> Result<ComputerEvidenceOutcome, SessionHubError> {
        let actor = self.actor_for(command.session_id.clone()).await?;
        let (completed, result) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::RecordComputerEvidence { command, completed })
            .await
            .map_err(|_| SessionHubError::Closed)?;
        result
            .await
            .map_err(|_| SessionHubError::Closed)?
            .map_err(Into::into)
    }

    pub(crate) async fn record_process_signal(
        &self,
        command: ProcessSignalCommand,
    ) -> Result<ProcessSignalOutcome, SessionHubError> {
        let actor = self.actor_for(command.session_id.clone()).await?;
        let (completed, result) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::RecordProcessSignal { command, completed })
            .await
            .map_err(|_| SessionHubError::Closed)?;
        result
            .await
            .map_err(|_| SessionHubError::Closed)?
            .map_err(Into::into)
    }

    async fn session_select_effort_receipt(
        &self,
        command_id: &CommandId,
        request_digest: &str,
        request_json: &str,
    ) -> Result<Option<SelectedEffort>, SessionHubError> {
        self.inner
            .store
            .session_select_effort_receipt(
                command_id.0.clone(),
                request_digest.to_owned(),
                request_json.to_owned(),
            )
            .await
            .map_err(Into::into)
    }

    /// The G3 effort selection, through the actor arm for the same reason as
    /// [`Self::select_session_model`]: the actor's in-memory head must
    /// advance with the committed fact or the compaction head CAS desyncs.
    pub(crate) async fn select_session_effort(
        &self,
        command: SessionSelectEffortCommand,
    ) -> Result<SessionSelectEffortOutcome, SessionHubError> {
        let actor = self.actor_for(command.session_id.clone()).await?;
        let (completed, result) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::SelectEffort { command, completed })
            .await
            .map_err(|_| SessionHubError::Closed)?;
        result
            .await
            .map_err(|_| SessionHubError::Closed)?
            .map_err(Into::into)
    }

    async fn session_select_agent_type_receipt(
        &self,
        command_id: &CommandId,
        request_digest: &str,
        request_json: &str,
    ) -> Result<Option<SelectedAgentType>, SessionHubError> {
        self.inner
            .store
            .session_select_agent_type_receipt(
                command_id.0.clone(),
                request_digest.to_owned(),
                request_json.to_owned(),
            )
            .await
            .map_err(Into::into)
    }

    /// The W-flow agent-type binding, through the actor arm (same head-CAS
    /// law as [`Self::select_session_effort`]).
    pub(crate) async fn select_session_agent_type(
        &self,
        command: SessionSelectAgentTypeCommand,
    ) -> Result<SessionSelectAgentTypeOutcome, SessionHubError> {
        let actor = self.actor_for(command.session_id.clone()).await?;
        let (completed, result) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::SelectAgentType { command, completed })
            .await
            .map_err(|_| SessionHubError::Closed)?;
        result
            .await
            .map_err(|_| SessionHubError::Closed)?
            .map_err(Into::into)
    }

    async fn session_select_fast_receipt(
        &self,
        command_id: &CommandId,
        request_digest: &str,
        request_json: &str,
    ) -> Result<Option<SelectedFast>, SessionHubError> {
        self.inner
            .store
            .session_select_fast_receipt(
                command_id.0.clone(),
                request_digest.to_owned(),
                request_json.to_owned(),
            )
            .await
            .map_err(Into::into)
    }

    /// The G3 fast-mode toggle, through the actor arm (same head-CAS law).
    pub(crate) async fn select_session_fast(
        &self,
        command: SessionSelectFastCommand,
    ) -> Result<SessionSelectFastOutcome, SessionHubError> {
        let actor = self.actor_for(command.session_id.clone()).await?;
        let (completed, result) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::SelectFast { command, completed })
            .await
            .map_err(|_| SessionHubError::Closed)?;
        result
            .await
            .map_err(|_| SessionHubError::Closed)?
            .map_err(Into::into)
    }

    async fn turn_accept_receipt(
        &self,
        command_id: &CommandId,
        request_digest: &str,
        request_json: &str,
    ) -> Result<Option<AcceptedTurn>, SessionHubError> {
        self.inner
            .store
            .turn_accept_receipt(
                command_id.0.clone(),
                request_digest.to_owned(),
                request_json.to_owned(),
            )
            .await
            .map_err(Into::into)
    }

    async fn accept_turn(
        &self,
        command: TurnAcceptCommand,
    ) -> Result<TurnAcceptOutcome, SessionHubError> {
        let actor = self.actor_for(command.session_id.clone()).await?;
        let (completed, result) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::AcceptTurn { command, completed })
            .await
            .map_err(|_| SessionHubError::Closed)?;
        result
            .await
            .map_err(|_| SessionHubError::Closed)?
            .map_err(Into::into)
    }

    async fn run_retry_receipt(
        &self,
        command_id: &CommandId,
        request_digest: &str,
        request_json: &str,
    ) -> Result<Option<AcceptedRunRetry>, SessionHubError> {
        self.inner
            .store
            .run_retry_receipt(
                command_id.0.clone(),
                request_digest.to_owned(),
                request_json.to_owned(),
            )
            .await
            .map_err(Into::into)
    }

    async fn accept_run_retry(
        &self,
        command: RunRetryCommand,
    ) -> Result<RunRetryOutcome, SessionHubError> {
        let actor = self.actor_for(command.session_id.clone()).await?;
        let (completed, result) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::AcceptRunRetry { command, completed })
            .await
            .map_err(|_| SessionHubError::Closed)?;
        result
            .await
            .map_err(|_| SessionHubError::Closed)?
            .map_err(Into::into)
    }

    async fn shell_exec_receipt(
        &self,
        command_id: &CommandId,
        request_digest: &str,
        request_json: &str,
    ) -> Result<Option<AcceptedShellExec>, SessionHubError> {
        self.inner
            .store
            .shell_exec_receipt(
                command_id.0.clone(),
                request_digest.to_owned(),
                request_json.to_owned(),
            )
            .await
            .map_err(Into::into)
    }

    async fn accept_shell_exec(
        &self,
        command: ShellExecAcceptCommand,
    ) -> Result<ShellExecAcceptOutcome, SessionHubError> {
        let actor = self.actor_for(command.session_id.clone()).await?;
        let (completed, result) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::AcceptShellExec { command, completed })
            .await
            .map_err(|_| SessionHubError::Closed)?;
        result
            .await
            .map_err(|_| SessionHubError::Closed)?
            .map_err(Into::into)
    }

    async fn turn_cancel_receipt(
        &self,
        command_id: &CommandId,
        request_digest: &str,
        request_json: &str,
    ) -> Result<Option<CancelledTurn>, SessionHubError> {
        self.inner
            .store
            .turn_cancel_receipt(
                command_id.0.clone(),
                request_digest.to_owned(),
                request_json.to_owned(),
            )
            .await
            .map_err(Into::into)
    }

    async fn cancel_turn(
        &self,
        command: TurnCancelCommand,
    ) -> Result<TurnCancelOutcome, SessionHubError> {
        let actor = self.actor_for(command.session_id.clone()).await?;
        let (completed, result) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::CancelTurn { command, completed })
            .await
            .map_err(|_| SessionHubError::Closed)?;
        result
            .await
            .map_err(|_| SessionHubError::Closed)?
            .map_err(Into::into)
    }

    /// Internal decision-hook route into the exact same actor/store menu CAS
    /// used by wire answers. This bypasses only connection capability checks;
    /// committed-menu identity, generation fencing, option validation, and
    /// first-winner arbitration remain unchanged.
    pub(crate) async fn resolve_hook_menu(
        &self,
        command: MenuResolutionCommand,
    ) -> Result<MenuResolutionOutcome, HaiderError> {
        let actor = self
            .actor_for(command.session_id.clone())
            .await
            .map_err(hub_error_as_store)?;
        let (completed, result) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::MenuAnswer { command, completed })
            .await
            .map_err(|_| hub_closed_store_error())?;
        result.await.map_err(|_| hub_closed_store_error())?
    }

    /// Mints and installs a same-process worker lease in actor order (R1).
    ///
    /// Installation REPLACES any current lease in the same serialized actor
    /// step, revoking the predecessor before the successor can append,
    /// register a harness, or receive cancellation wakes (the fencing law is
    /// stated in actor.rs's charter). The returned [`HubStoreHandle`] is the
    /// ONLY store surface a worker may hold. Refused while draining: a new
    /// worker is new admission under the R9 gate.
    pub async fn acquire_worker_lease(
        &self,
        session_id: SessionId,
    ) -> Result<HubStoreHandle, SessionHubError> {
        self.acquire_worker_lease_inner(session_id, None).await
    }

    pub(crate) async fn acquire_worker_lease_with_cancellation_wake(
        &self,
        session_id: SessionId,
        cancellation_wake: watch::Sender<u64>,
    ) -> Result<HubStoreHandle, SessionHubError> {
        self.acquire_worker_lease_inner(session_id, Some(cancellation_wake))
            .await
    }

    pub(crate) async fn observe_child_template_success(
        &self,
        command: ChildTemplateObservationCommand,
    ) -> Result<ChildTemplateObservation, SessionHubError> {
        let actor = self.actor_for(command.parent_session_id.clone()).await?;
        let (completed, result) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::ObserveChildTemplate { command, completed })
            .await
            .map_err(|_| SessionHubError::Closed)?;
        result
            .await
            .map_err(|_| SessionHubError::Closed)?
            .map_err(Into::into)
    }

    pub(crate) async fn child_template_cache_lookup(
        &self,
        key: haider_protocol::graph::ChildTemplateCacheKey,
    ) -> Result<Option<ChildTemplateCacheEntry>, HaiderError> {
        self.inner.store.child_template_cache_lookup(key).await
    }

    /// Manager-only drain seam. External admission is already closed, but an
    /// acceptance actor may contain a durable Queued run whose post-commit
    /// manager hint lost the drain race. After supervisors join, the manager
    /// may mint one final lease on that EXISTING actor solely to sweep those
    /// runs to a terminal state.
    pub(crate) async fn acquire_drain_worker_lease(
        &self,
        session_id: SessionId,
    ) -> Result<Option<HubStoreHandle>, SessionHubError> {
        let Some(actor) = self.existing_actor(&session_id)? else {
            return Ok(None);
        };
        let lease_id = WorkerLeaseId(random_id("drain-worker-lease")?);
        let (completed, response) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::AcquireWorkerLease {
                lease_id: lease_id.clone(),
                cancellation_wake: None,
                completed,
            })
            .await
            .map_err(|_| SessionHubError::Closed)?;
        response.await.map_err(|_| SessionHubError::Closed)?;
        Ok(Some(HubStoreHandle {
            hub: self.clone(),
            session_id,
            worker_generation: self.inner.store.worker_generation(),
            lease_id,
        }))
    }

    pub(crate) async fn session_ids(&self) -> Result<Vec<SessionId>, SessionHubError> {
        self.inner.store.session_ids().await.map_err(Into::into)
    }

    /// Deletes one quiesced session through the daemon's production
    /// lifecycle. New actor admission is fenced first; attached or
    /// nonterminal sessions are refused. The actor stops before the durable
    /// transaction, and ephemeral handoff data is cleaned only after commit.
    pub async fn delete_session(&self, session_id: SessionId) -> Result<(), HaiderError> {
        {
            let mut deleting = lock(&self.inner.deleting_sessions).map_err(hub_error_as_store)?;
            if !deleting.insert(session_id.clone()) {
                return Err(HaiderError::new(
                    ErrorCode::Busy,
                    "session deletion is already in progress",
                    true,
                ));
            }
        }
        let result = self.delete_fenced_session(&session_id).await;
        if result.is_err()
            && let Ok(mut deleting) = lock(&self.inner.deleting_sessions)
        {
            deleting.remove(&session_id);
        }
        result
    }

    async fn delete_fenced_session(&self, session_id: &SessionId) -> Result<(), HaiderError> {
        let metadata = self
            .inner
            .store
            .session_metadata(session_id)
            .await?
            .ok_or_else(|| {
                HaiderError::new(ErrorCode::InvalidArgument, "session was not found", false)
            })?;
        if lock(&self.inner.attachments)
            .map_err(hub_error_as_store)?
            .values()
            .any(|owner| owner.session_id == *session_id)
        {
            return Err(HaiderError::new(
                ErrorCode::Busy,
                "attached session cannot be deleted",
                true,
            ));
        }
        if self.session_has_nonterminal_runs(session_id).await? {
            return Err(HaiderError::new(
                ErrorCode::Busy,
                "nonterminal session cannot be deleted",
                true,
            ));
        }
        let actor = lock(&self.inner.actors)
            .map_err(hub_error_as_store)?
            .get(session_id)
            .cloned();
        if let Some(actor) = actor {
            let (completed, quiescent) = oneshot::channel();
            actor
                .commands
                .send(ActorCommand::StopIfQuiescent { completed })
                .await
                .map_err(|_| {
                    HaiderError::new(
                        ErrorCode::Internal,
                        "session actor stopped before deletion fencing",
                        true,
                    )
                })?;
            if !quiescent.await.map_err(|_| {
                HaiderError::new(
                    ErrorCode::Internal,
                    "session actor did not acknowledge deletion fencing",
                    true,
                )
            })?? {
                return Err(HaiderError::new(
                    ErrorCode::Busy,
                    "session became nonterminal or attached during deletion",
                    true,
                ));
            }
            // The actor stopped only after its FIFO-local quiescence check.
            // The deletion tombstone prevents any replacement from racing
            // this removal.
            lock(&self.inner.actors)
                .map_err(hub_error_as_store)?
                .remove(session_id);
        }
        // W-A fence law: session close kills every pgid the session owns,
        // after the actor is provably stopped and before the durable delete.
        self.fence_background_tasks(session_id).await;
        self.inner.store.delete_session(session_id.clone()).await?;
        self.inner.observe_digests.remove(session_id);
        if let Ok(Some(hooks)) = self.hooks() {
            hooks.session_deleted(session_id.clone());
        }
        let _ = self.inner.roster_publications.send(session_id.clone());
        self.clear_session_surface(session_id);
        crate::delegation::DelegationHandle::new(self.clone())
            .cleanup_handoff_for_deleted_parent(&metadata.cwd, session_id)
            .await;
        Ok(())
    }

    async fn session_has_nonterminal_runs(
        &self,
        session_id: &SessionId,
    ) -> Result<bool, HaiderError> {
        let mut cursor = 0;
        let mut states = HashMap::<RunId, RunState>::new();
        loop {
            let page = self.inner.store.read(session_id, cursor, 256).await?;
            if page.is_empty() {
                return Ok(states.values().any(|state| !state.is_terminal()));
            }
            cursor = page.last().map_or(cursor, |event| event.seq);
            for event in page {
                let Some(run_id) = event.run_id else {
                    continue;
                };
                if let Ok(EventPayload::RunState(state)) =
                    serde_json::from_value::<EventPayload>(event.payload)
                {
                    states.insert(run_id, state);
                }
            }
        }
    }

    async fn acquire_worker_lease_inner(
        &self,
        session_id: SessionId,
        cancellation_wake: Option<watch::Sender<u64>>,
    ) -> Result<HubStoreHandle, SessionHubError> {
        let actor = self.actor_for(session_id.clone()).await?;
        let lease_id = WorkerLeaseId(random_id("worker-lease")?);
        let (completed, response) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::AcquireWorkerLease {
                lease_id: lease_id.clone(),
                cancellation_wake,
                completed,
            })
            .await
            .map_err(|_| SessionHubError::Closed)?;
        response.await.map_err(|_| SessionHubError::Closed)?;
        Ok(HubStoreHandle {
            hub: self.clone(),
            session_id,
            worker_generation: self.inner.store.worker_generation(),
            lease_id,
        })
    }

    /// Installs the harness that committed menu resolutions wake, under the
    /// caller's still-current lease; a superseded lease is rejected instead
    /// of overwriting its successor's registration.
    /// Returns the already-running actor, if any — deliberately WITHOUT the
    /// draining check `actor_for` applies. This asymmetry is the hub's half
    /// of the R9 drain gate: `begin_draining` closes every actor-CREATING
    /// path (external requests, new leases), while a worker that already
    /// holds a lease keeps reaching its existing actor so its final
    /// `Cancelled`/effect/idle appends commit and publish during the grace.
    fn existing_actor(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionActorHandle>, SessionHubError> {
        Ok(lock(&self.inner.actors)?.get(session_id).cloned())
    }

    /// Returns the session's actor, creating it on first use. Refused while
    /// draining — this is the actor-creating half of the R9 admission gate
    /// (see `existing_actor` for the worker-side exception).
    async fn actor_for(
        &self,
        session_id: SessionId,
    ) -> Result<SessionActorHandle, SessionHubError> {
        if self.inner.draining.load(Ordering::Acquire) {
            return Err(SessionHubError::Closed);
        }
        if lock(&self.inner.deleting_sessions)?.contains(&session_id) {
            return Err(SessionHubError::Store(HaiderError::new(
                ErrorCode::InvalidArgument,
                "session was deleted",
                false,
            )));
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
        if lock(&self.inner.deleting_sessions)?.contains(&session_id) {
            return Err(SessionHubError::Store(HaiderError::new(
                ErrorCode::InvalidArgument,
                "session was deleted",
                false,
            )));
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
            self.inner.append_committer.clone(),
            Arc::clone(&self.inner.pipe_native),
            Arc::clone(&self.inner.observer),
            Arc::clone(&self.inner.metrics),
            Arc::clone(&self.inner.commit_projection),
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
        // Serialize the final owner publication with deletion-fence minting.
        // A pre-fence register that returns from the actor after deletion
        // started must detach/refuse instead of appearing behind the deleter's
        // attachment check.
        let deleting = lock(&self.inner.deleting_sessions)?;
        if deleting.contains(&session_id) {
            drop(deleting);
            let _ = actor.commands.try_send(ActorCommand::Detach {
                attachment_id: attachment_id.clone(),
            });
            return Err(SessionHubError::Store(HaiderError::new(
                ErrorCode::InvalidArgument,
                "session was deleted",
                false,
            )));
        }
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
        drop(deleting);
        self.notify_haider_code_plan_change();
        Ok(RegisterResult::Registered(registration))
    }

    fn spawn_replay(
        &self,
        registration: Registration,
        after_seq: u64,
        sealed_replay: bool,
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
        let task = tokio::spawn(run_replay(
            hub,
            registration,
            after_seq,
            sealed_replay,
            sink,
            cancel,
        ));
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
            self.notify_haider_code_plan_change();
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

    fn live_surface_snapshot(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionSurfaceSnapshot>, SessionHubError> {
        let deleting = lock(&self.inner.deleting_sessions)?;
        if deleting.contains(session_id) {
            return Ok(None);
        }
        let mut surfaces = self
            .inner
            .surfaces
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = surfaces.entry(session_id.clone()).or_default();
        Ok(Some(SessionSurfaceSnapshot {
            input: state.input.clone(),
            status: state.status.clone(),
            change_generation: state.change_generation,
        }))
    }

    /// Snapshots one session's surface only when its change generation moved
    /// past `seen_generation`. The idle-tick common case compares under the
    /// lock and clones nothing; delivery semantics are unchanged because the
    /// caller previously discarded the equal-generation clone unsent.
    fn surface_snapshot_if_changed(
        &self,
        session_id: &SessionId,
        seen_generation: u64,
    ) -> Option<SessionSurfaceSnapshot> {
        let surfaces = self
            .inner
            .surfaces
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = surfaces.get(session_id);
        let change_generation = state.map_or(0, |state| state.change_generation);
        if change_generation == seen_generation {
            return None;
        }
        Some(SessionSurfaceSnapshot {
            input: state.and_then(|state| state.input.clone()),
            status: state.and_then(|state| state.status.clone()),
            change_generation,
        })
    }

    fn notify_surface_watchers(&self) {
        self.inner
            .surface_publications
            .send_modify(|generation| *generation = generation.saturating_add(1));
    }

    fn publish_surface(
        &self,
        connection_id: &str,
        session_id: &SessionId,
        input: Option<SurfaceInputPublishWire>,
        status: Option<SurfaceStatusPublishWire>,
    ) -> Result<Option<SurfacePublishOutcome>, SessionHubError> {
        // Serialize the final volatile publication with deletion-fence
        // minting, just as attachment ownership does. A publication that
        // wins this lock is cleared by the later successful delete; one that
        // loses observes the permanent daemon-lifetime tombstone.
        let deleting = lock(&self.inner.deleting_sessions)?;
        if deleting.contains(session_id) {
            return Ok(None);
        }
        let mut surfaces = self
            .inner
            .surfaces
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = surfaces.entry(session_id.clone()).or_default();
        let mut accepted_input_revision = None;
        let mut accepted_status_revision = None;

        if let Some(input) = input
            && state
                .input_revisions
                .get(connection_id)
                .is_none_or(|revision| input.revision > *revision)
        {
            state
                .input_revisions
                .insert(connection_id.to_owned(), input.revision);
            accepted_input_revision = Some(input.revision);
            state.input = Some(SurfaceInputWire {
                text: input.text,
                attachments: input.attachments,
                revision: input.revision,
                owner: connection_id.to_owned(),
            });
        }
        if let Some(status) = status
            && state
                .status_revisions
                .get(connection_id)
                .is_none_or(|revision| status.revision > *revision)
        {
            state
                .status_revisions
                .insert(connection_id.to_owned(), status.revision);
            accepted_status_revision = Some(status.revision);
            state.status = Some(SurfaceStatusWire {
                line: status.line,
                state: status.state,
                detail: status.detail,
                revision: status.revision,
                owner: connection_id.to_owned(),
            });
        }
        if accepted_input_revision.is_some() || accepted_status_revision.is_some() {
            state.change_generation = state.change_generation.saturating_add(1);
        }
        Ok(Some(SurfacePublishOutcome {
            accepted_input_revision,
            accepted_status_revision,
        }))
    }

    fn inject_session_input(
        &self,
        session_id: &SessionId,
        op: SurfaceInjectOp,
    ) -> Result<bool, SessionHubError> {
        let sink = {
            let deleting = lock(&self.inner.deleting_sessions)?;
            if deleting.contains(session_id) {
                return Ok(false);
            }
            let surfaces = self
                .inner
                .surfaces
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(owner) = surfaces
                .get(session_id)
                .and_then(|state| state.input.as_ref())
                .map(|input| input.owner.as_str())
            else {
                return Ok(false);
            };
            lock(&self.inner.diagnostic_sinks)?.get(owner).cloned()
        };
        let Some(sink) = sink else {
            return Ok(false);
        };
        Ok(sink
            .try_send_droppable(WireFrame::SessionInputInjected {
                session_id: session_id.clone(),
                op,
            })
            .is_ok())
    }

    fn clear_surface_owner(&self, connection_id: &str) {
        let mut surfaces = self
            .inner
            .surfaces
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut changed = false;
        for state in surfaces.values_mut() {
            state.input_revisions.remove(connection_id);
            state.status_revisions.remove(connection_id);
            let clears_input = state
                .input
                .as_ref()
                .is_some_and(|input| input.owner == connection_id);
            let clears_status = state
                .status
                .as_ref()
                .is_some_and(|status| status.owner == connection_id);
            if clears_input {
                state.input = None;
            }
            if clears_status {
                state.status = None;
            }
            if clears_input || clears_status {
                state.change_generation = state.change_generation.saturating_add(1);
                changed = true;
            }
        }
        drop(surfaces);
        if changed {
            self.notify_surface_watchers();
        }
    }

    /// Atomically installs and publishes one resident TUI's binding. The most
    /// recently announced live publisher is the visible profile state; if it
    /// exits, the previous live publisher is restored. Holding this registry
    /// lock through sink admission establishes a total order with
    /// late-subscriber baselines. This signal must never use the droppable
    /// lane.
    fn publish_resident_binding(
        &self,
        source_connection_id: &str,
        session_id: Option<SessionId>,
        worker_generation: u64,
        binding_token: Option<String>,
    ) -> Result<(), SessionHubError> {
        let mut registry = lock(&self.inner.resident_binding)?;
        let previous = registry.visible();
        registry.next_revision = registry.next_revision.saturating_add(1);
        let revision = registry.next_revision;
        registry.publishers.insert(
            source_connection_id.to_owned(),
            ResidentBindingState {
                session_id: session_id.clone(),
                worker_generation,
                binding_token: binding_token.clone(),
                revision,
            },
        );
        registry.vacant_generation = None;
        let next = registry.visible();
        if previous == next {
            return Ok(());
        }
        let frame = WireFrame::ResidentSessionBinding {
            session_id,
            worker_generation,
            binding_token,
        };
        let viewers = lock(&self.inner.resident_binding_viewers)?;
        let sinks = lock(&self.inner.diagnostic_sinks)?;
        for (connection_id, sink) in sinks.iter() {
            if connection_id == source_connection_id || !viewers.contains(connection_id) {
                continue;
            }
            if sink.try_send(frame.clone()).is_err() {
                sink.close_after_required_delivery_failure();
            }
        }
        Ok(())
    }

    /// Removes one publisher. Closing an overlapped old connection is a
    /// no-op at the visible layer; closing the current publisher either
    /// restores the most recent live predecessor or emits explicit unbind.
    fn clear_resident_binding(&self, source_connection_id: &str) {
        let Ok(mut registry) = self.inner.resident_binding.lock() else {
            return;
        };
        let previous_owner = registry.current().map(|(owner, _)| owner.to_owned());
        let previous = registry.visible();
        let Some(removed) = registry.publishers.remove(source_connection_id) else {
            return;
        };
        if previous_owner.as_deref() != Some(source_connection_id) {
            return;
        }
        if registry.publishers.is_empty() {
            registry.vacant_generation = Some(removed.worker_generation);
        }
        let next = registry.visible();
        if previous == next {
            return;
        }
        let (session_id, worker_generation, binding_token) =
            next.unwrap_or((None, removed.worker_generation, None));
        let frame = WireFrame::ResidentSessionBinding {
            session_id,
            worker_generation,
            binding_token,
        };
        let Ok(viewers) = self.inner.resident_binding_viewers.lock() else {
            return;
        };
        let Ok(sinks) = self.inner.diagnostic_sinks.lock() else {
            return;
        };
        for (connection_id, sink) in sinks.iter() {
            if connection_id == source_connection_id || !viewers.contains(connection_id) {
                continue;
            }
            if sink.try_send(frame.clone()).is_err() {
                sink.close_after_required_delivery_failure();
            }
        }
    }

    fn clear_session_surface(&self, session_id: &SessionId) {
        let mut surfaces = self
            .inner
            .surfaces
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = surfaces.entry(session_id.clone()).or_default();
        state.input = None;
        state.status = None;
        state.input_revisions.clear();
        state.status_revisions.clear();
        // Session death itself is a change even when both fields were empty;
        // a registered watcher must receive the cleared terminal snapshot.
        state.change_generation = state.change_generation.saturating_add(1);
        drop(surfaces);
        self.notify_surface_watchers();
    }

    fn detach_connection_registrations(
        &self,
        connection_id: &str,
    ) -> Result<Vec<(AttachmentId, AttachmentOwner)>, SessionHubError> {
        lock(&self.inner.resident_binding_viewers)?.remove(connection_id);
        lock(&self.inner.diagnostic_sinks)?.remove(connection_id);
        self.clear_surface_owner(connection_id);
        let attachment_ids = {
            let owners = lock(&self.inner.attachments)?;
            owners
                .iter()
                .filter(|(_, owner)| owner.connection_id == connection_id)
                .map(|(attachment_id, _)| attachment_id.clone())
                .collect::<Vec<_>>()
        };
        let mut attachments = Vec::with_capacity(attachment_ids.len());
        for attachment_id in attachment_ids {
            if let Some(owner) = self.take_attachment(&attachment_id, Some(connection_id))? {
                attachments.push((attachment_id, owner));
            }
        }
        Ok(attachments)
    }

    async fn detach_connection(&self, connection_id: &str) -> Result<(), SessionHubError> {
        let attachments = self.detach_connection_registrations(connection_id)?;
        for (attachment_id, owner) in attachments {
            Self::finish_detach(&attachment_id, owner).await;
        }
        Ok(())
    }

    /// True when `connection_id` holds a CONTROL attachment to `session_id`.
    ///
    /// CONTROL-ATTACHMENT POLICY (authoritative statement, R7/§5.7):
    /// session-scoped mutation — `turn.submit`, `turn.cancel`, and
    /// `MenuAnswer` — requires the Control capability AND a live CONTROL
    /// attachment to the target session; v0.1 has no controller-without-
    /// viewport allowance. `session.create` is exempt only because the
    /// session does not exist yet. The rpc.rs guards all call this one
    /// predicate; do not restate the rule inline.
    fn holds_control_attachment(
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

    fn offer_attachment_prepared(
        &self,
        attachment_id: &AttachmentId,
        sink: &Arc<dyn FrameSink>,
        frame: &PreparedFrame,
    ) -> SendAdmission {
        let Ok(attachments) = lock(&self.inner.attachments) else {
            return SendAdmission::Refused;
        };
        if !attachments.contains_key(attachment_id) {
            return SendAdmission::Refused;
        }
        // The ownership lock makes admit-vs-detach atomic: detach removes the
        // owner before purging its lane, so no frame can appear after purge.
        sink.offer_prepared(attachment_id, frame)
    }

    fn offer_attachment_prepared_ticketed(
        &self,
        attachment_id: &AttachmentId,
        sink: &Arc<dyn FrameSink>,
        frame: &PreparedFrame,
        ticket: &AdmissionTicket,
    ) -> SendAdmission {
        let Ok(attachments) = lock(&self.inner.attachments) else {
            return SendAdmission::Refused;
        };
        if !attachments.contains_key(attachment_id) {
            return SendAdmission::Refused;
        }
        // Keep the same admit-vs-detach ownership barrier as the fresh offer.
        sink.offer_prepared_ticketed(attachment_id, frame, ticket)
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
    ///
    /// DELIVERED-OR-FORCED LAW: replay tasks report terminal suffix status
    /// into this join. Any failure to read the durable suffix or enqueue its
    /// final marker is recorded and returns [`SessionHubShutdownOutcome::Forced`];
    /// it can never be mistaken for a graceful join.
    pub async fn shutdown(&self) -> Result<SessionHubShutdownOutcome, SessionHubError> {
        // W-A fence law: background tasks die with the daemon. The kill
        // ladders run BEFORE the drain flag so completion facts can still
        // journal; anything unsettled is reaped by next-start adoption.
        self.shutdown_background_tasks().await;
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
        let append_commit_task = lock(&self.inner.append_commit_task)?.take();
        let mut append_commit_task = OwnedTasks::new(
            append_commit_task.into_iter().collect(),
            Arc::clone(&self.inner.force_stop),
        );
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
        let _ = actor_tasks.join_all().await;
        self.inner.append_committer.shutdown().await;
        let append_outcomes = append_commit_task.join_all().await;
        if append_outcomes.iter().any(Result::is_err) {
            return Err(SessionHubError::Task(
                "append group-commit task failed during shutdown".into(),
            ));
        }
        self.inner
            .observer
            .observe(HubObservation::ShutdownActorsStopped);
        // Actor death drops every catch-up sender; each replay task drains
        // what was already buffered, streams it into its sink, and exits on
        // the closed channel. Join WITHOUT aborting so those final committed
        // envelopes reach the connection outboxes (§6.6's final-broadcast
        // grace; the write side runs under the connection drain deadline).
        let replay_outcomes = replay_tasks.join_all().await;
        let mut outcome = SessionHubShutdownOutcome::Graceful;
        let mut join_failure = None;
        for replay_outcome in replay_outcomes {
            match replay_outcome {
                Ok(ReplayCompletion::Complete) => {}
                Ok(ReplayCompletion::FinalSuffixFailed(failure)) => {
                    tracing::error!(
                        stage = failure.stage,
                        error = %failure.message,
                        "final attachment suffix was not delivered during shutdown; forcing outcome"
                    );
                    outcome = SessionHubShutdownOutcome::Forced;
                }
                Err(error) => {
                    tracing::error!(
                        error = ?error,
                        "attachment replay task failed while shutdown was joining it"
                    );
                    join_failure.get_or_insert_with(|| {
                        SessionHubError::Task(format!(
                            "attachment replay task failed during shutdown: {error}"
                        ))
                    });
                }
            }
        }
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
        // Every task joined in order, so the abort fence stays down even when
        // a recorded suffix failure makes the reported outcome Forced.
        forced.armed = false;
        match join_failure {
            Some(error) => Err(error),
            None => Ok(outcome),
        }
    }
}

/// The worker-facing store surface. Reads go straight to the store: committed
/// history needs no actor serialization.
#[async_trait]
impl StoreHandle for HubStoreHandle {
    async fn append(
        &self,
        envelopes: &mut [RawEnvelope],
    ) -> Result<haider_core::CommittedRange, HaiderError> {
        let Some(first) = envelopes.first() else {
            return Err(HaiderError::new(
                ErrorCode::InvalidArgument,
                "cannot append an empty worker envelope batch",
                false,
            ));
        };
        if envelopes.iter().any(|envelope| {
            envelope.session_id != self.session_id
                || envelope.worker_generation != self.worker_generation
        }) {
            return Err(HaiderError::new(
                ErrorCode::SingleWriterViolation,
                "worker envelope identity does not match its lease",
                false,
            ));
        }
        if inject_test_done_append_failure(envelopes) {
            return Err(HaiderError::new(
                ErrorCode::StoreCorrupt,
                "injected terminal append failure",
                false,
            ));
        }
        let actor = self
            .hub
            .existing_actor(&first.session_id)
            .map_err(hub_error_as_store)?
            .ok_or_else(hub_closed_store_error)?;
        let (completed, response) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::WorkerAppend {
                lease_id: self.lease_id.clone(),
                expected_head: None,
                envelopes: envelopes.to_vec(),
                completed,
            })
            .await
            .map_err(|_| hub_closed_store_error())?;
        let committed = response.await.map_err(|_| hub_closed_store_error())??;
        envelopes.clone_from_slice(&committed);
        Ok(haider_core::CommittedRange {
            first_seq: committed.first().map_or(0, |envelope| envelope.seq),
            last_seq: committed.last().map_or(0, |envelope| envelope.seq),
        })
    }

    async fn read(
        &self,
        session_id: &SessionId,
        since_seq: u64,
        limit: usize,
    ) -> Result<Vec<RawEnvelope>, HaiderError> {
        self.ensure_session(session_id)?;
        self.hub
            .inner
            .store
            .read(session_id, since_seq, limit)
            .await
    }

    async fn latest_seq(&self, session_id: &SessionId) -> Result<u64, HaiderError> {
        self.ensure_session(session_id)?;
        self.hub.inner.store.latest_seq(session_id).await
    }

    async fn projection_checkpoint(
        &self,
        session_id: &SessionId,
        projection: &str,
        timeline_key: &str,
    ) -> Result<Option<SessionProjectionCheckpoint>, HaiderError> {
        self.ensure_session(session_id)?;
        self.hub
            .inner
            .store
            .projection_checkpoint(session_id, projection.to_owned(), timeline_key.to_owned())
            .await
    }

    async fn put_projection_checkpoint(
        &self,
        checkpoint: SessionProjectionCheckpoint,
    ) -> Result<(), HaiderError> {
        self.ensure_session(&checkpoint.session_id)?;
        self.hub
            .inner
            .store
            .put_projection_checkpoint(checkpoint)
            .await
    }

    async fn branch_lineage(
        &self,
        session_id: &SessionId,
        branch_id: Option<&BranchId>,
    ) -> Result<Vec<BranchDescriptor>, HaiderError> {
        self.ensure_session(session_id)?;
        StoreHandle::branch_lineage(&self.hub.inner.store, session_id, branch_id).await
    }
}

/// One-shot migration-oracle seam for the daemon-backed CLI test. It is
/// inert unless the daemon is already running the explicit injected fake
/// provider and the dedicated fault variable is set in that child process.
fn inject_test_done_append_failure(envelopes: &[RawEnvelope]) -> bool {
    static FAIL_NEXT_DONE: OnceLock<AtomicBool> = OnceLock::new();
    let armed = FAIL_NEXT_DONE.get_or_init(|| {
        AtomicBool::new(
            std::env::var_os("HAIDER_TEST_FAKE_PROVIDER").is_some()
                && std::env::var_os("HAIDER_TEST_FAIL_NEXT_DONE_APPEND").is_some(),
        )
    });
    if !armed.load(Ordering::Acquire)
        || !envelopes.iter().any(|envelope| {
            serde_json::from_value::<EventPayload>(envelope.payload.clone())
                .is_ok_and(|payload| payload == EventPayload::RunState(RunState::Done))
        })
    {
        return false;
    }
    armed.swap(false, Ordering::AcqRel)
}

impl HubStoreHandle {
    /// Appends a worker batch only while the session journal still has the
    /// exact head observed by the caller. The session actor performs the
    /// comparison and append without yielding to another session command.
    pub(crate) async fn append_at_head(
        &self,
        expected_head: u64,
        envelopes: &mut [RawEnvelope],
    ) -> Result<haider_core::CommittedRange, HaiderError> {
        let Some(first) = envelopes.first() else {
            return Err(HaiderError::new(
                ErrorCode::InvalidArgument,
                "cannot append an empty worker envelope batch",
                false,
            ));
        };
        if envelopes.iter().any(|envelope| {
            envelope.session_id != self.session_id
                || envelope.worker_generation != self.worker_generation
        }) {
            return Err(HaiderError::new(
                ErrorCode::SingleWriterViolation,
                "worker envelope identity does not match its lease",
                false,
            ));
        }
        let actor = self
            .hub
            .existing_actor(&first.session_id)
            .map_err(hub_error_as_store)?
            .ok_or_else(hub_closed_store_error)?;
        let (completed, response) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::WorkerAppend {
                lease_id: self.lease_id.clone(),
                expected_head: Some(expected_head),
                envelopes: envelopes.to_vec(),
                completed,
            })
            .await
            .map_err(|_| hub_closed_store_error())?;
        let committed = response.await.map_err(|_| hub_closed_store_error())??;
        envelopes.clone_from_slice(&committed);
        Ok(haider_core::CommittedRange {
            first_seq: committed.first().map_or(0, |envelope| envelope.seq),
            last_seq: committed.last().map_or(0, |envelope| envelope.seq),
        })
    }

    pub(crate) async fn claim_context_compaction_receipt(
        &self,
        command_id: String,
        request_digest: String,
        request_json: String,
    ) -> Result<haider_core::ContextCompactionClaim, HaiderError> {
        self.hub
            .inner
            .store
            .claim_context_compaction_receipt(command_id, request_digest, request_json)
            .await
    }

    pub(crate) async fn finalize_context_compaction_receipt(
        &self,
        command_id: String,
        response: haider_core::ContextCompactionReceiptResponse,
    ) -> Result<(), HaiderError> {
        self.hub
            .inner
            .store
            .finalize_context_compaction_receipt(command_id, response)
            .await
    }
}

#[async_trait::async_trait]
impl haider_core::ArtifactReader for HubStoreHandle {
    async fn read_artifact(
        &self,
        artifact: &haider_protocol::ids::ArtifactRef,
    ) -> Result<Vec<u8>, HaiderError> {
        self.get_artifact(artifact.clone()).await
    }
}

impl HubStoreHandle {
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// The owning hub — used by the worker to build hub-backed facades
    /// (background tasks) without widening the lease surface.
    pub(crate) fn hub(&self) -> &SessionHub {
        &self.hub
    }

    pub(crate) async fn compile_prompt_projection(
        &self,
        branch_id: Option<&BranchId>,
        agent_id: Option<&haider_protocol::ids::AgentId>,
        current_run: &RunId,
    ) -> Result<haider_core::CompiledPromptProjection, HaiderError> {
        haider_core::PromptHistoryCompiler::compile_cached_provider_projection_with_artifacts(
            &self.hub.inner.prompt_history,
            self,
            self,
            &self.session_id,
            branch_id,
            agent_id,
            current_run,
        )
        .await
    }

    pub fn worker_generation(&self) -> u64 {
        self.worker_generation
    }

    /// Reads THIS lease's session metadata fresh from the store. The handle
    /// cannot name another session, so the read carries the same structural
    /// R1 seal as appends. The turn supervisor calls this before every turn
    /// start so a committed `session.select_model` is picked up by the next
    /// logical turn without a worker restart (R6 re-resolution).
    pub(crate) async fn session_metadata(
        &self,
    ) -> Result<Option<haider_protocol::session::SessionMetadataV1>, HaiderError> {
        self.hub
            .inner
            .store
            .session_metadata(&self.session_id)
            .await
    }

    /// Installs this lease's harness. The receiver cannot name another
    /// session or lease, so registration carries the same structural R1 seal
    /// as reads and appends.
    pub async fn register_harness(&self, harness: HarnessHandle) -> Result<(), SessionHubError> {
        let actor = self.hub.actor_for(self.session_id.clone()).await?;
        let (completed, response) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::RegisterHarness {
                lease_id: self.lease_id.clone(),
                harness,
                completed,
            })
            .await
            .map_err(|_| SessionHubError::Closed)?;
        response
            .await
            .map_err(|_| SessionHubError::Closed)?
            .map_err(Into::into)
    }

    /// Registers the prior-generation menu coordinate reconstructed for this
    /// lease's session; no cross-session facade reaches the worker.
    pub async fn register_recovered_harness(
        &self,
        harness: HarnessHandle,
        menu_id: MenuId,
        request_seq: u64,
        opening_generation: u64,
    ) -> Result<(), SessionHubError> {
        let actor = self.hub.actor_for(self.session_id.clone()).await?;
        let (completed, response) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::RegisterRecoveredHarness {
                lease_id: self.lease_id.clone(),
                harness,
                menu: RecoveredMenuCoordinate {
                    menu_id,
                    request_seq,
                    opening_generation,
                },
                completed,
            })
            .await
            .map_err(|_| SessionHubError::Closed)?;
        response
            .await
            .map_err(|_| SessionHubError::Closed)?
            .map_err(Into::into)
    }

    /// Releases this lease's registration if it is still current. A
    /// superseded token is a silent no-op, so predecessor cleanup cannot
    /// unregister its successor.
    pub async fn unregister_worker(&self) -> Result<(), SessionHubError> {
        let actor = self
            .hub
            .existing_actor(&self.session_id)?
            .ok_or(SessionHubError::Closed)?;
        actor
            .commands
            .send(ActorCommand::UnregisterHarness {
                lease_id: self.lease_id.clone(),
            })
            .await
            .map_err(|_| SessionHubError::Closed)
    }

    pub(crate) async fn put_artifact(
        &self,
        bytes: Vec<u8>,
    ) -> Result<haider_protocol::ids::ArtifactRef, HaiderError> {
        self.hub.inner.store.put(bytes).await
    }

    pub(crate) async fn put_artifact_file(
        &self,
        path: std::path::PathBuf,
    ) -> Result<haider_protocol::ids::ArtifactRef, HaiderError> {
        self.hub.inner.store.put_file(path).await
    }

    pub(crate) async fn put_image_artifact(
        &self,
        bytes: Vec<u8>,
        media_type: String,
    ) -> Result<haider_protocol::tool::ImageBlockRef, HaiderError> {
        self.hub.inner.store.put_image(bytes, media_type).await
    }

    pub(crate) async fn get_artifact(
        &self,
        artifact: haider_protocol::ids::ArtifactRef,
    ) -> Result<Vec<u8>, HaiderError> {
        self.hub.inner.store.get(&artifact).await
    }

    pub(crate) async fn settle_idle(
        &self,
        envelope: RawEnvelope,
    ) -> Result<Option<RawEnvelope>, HaiderError> {
        if envelope.session_id != self.session_id
            || envelope.worker_generation != self.worker_generation
        {
            return Err(HaiderError::new(
                ErrorCode::SingleWriterViolation,
                "aggregate-state envelope identity does not match its worker lease",
                false,
            ));
        }
        let actor = self
            .hub
            .existing_actor(&self.session_id)
            .map_err(hub_error_as_store)?
            .ok_or_else(hub_closed_store_error)?;
        let (completed, response) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::WorkerSettleIdle {
                lease_id: self.lease_id.clone(),
                envelope,
                completed,
            })
            .await
            .map_err(|_| hub_closed_store_error())?;
        response.await.map_err(|_| hub_closed_store_error())?
    }

    fn ensure_session(&self, session_id: &SessionId) -> Result<(), HaiderError> {
        if *session_id == self.session_id {
            Ok(())
        } else {
            Err(HaiderError::new(
                ErrorCode::SingleWriterViolation,
                "worker lease cannot access another session",
                false,
            ))
        }
    }
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
