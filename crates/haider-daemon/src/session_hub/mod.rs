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
mod descendant_stream;
mod replay;
pub(crate) mod rpc;
#[cfg(test)]
pub(crate) use rpc::pdf_delivery_for_provider;

#[cfg(test)]
pub(crate) async fn open_retention_test_hub(
    path: &std::path::Path,
) -> Result<(SqliteStoreHandle, SessionHub), SessionHubError> {
    let store = SqliteStoreHandle::open(path).await?;
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default())?;
    Ok((store, hub))
}

use crate::DaemonError;
use crate::worker::{TurnSetupReductionCache, WorkerManagerHandle};
use actor::run_session_actor;
use async_trait::async_trait;
use base64::Engine as _;
use descendant_stream::run_descendant_stream;
use haider_core::{
    AbandonedGraph, AcceptedRunRetry, AcceptedShellExec, AcceptedTurn, AppendGroupBatch,
    BranchCreateCommand, BranchCreateOutcome, CacheDiagnosticKey, CancelledTurn,
    CheckpointCommitCommand, CheckpointCommitOutcome, ChildGraphAttachCommand,
    ChildGraphAttachOutcome, ChildTemplateCacheEntry, ChildTemplateObservation,
    ChildTemplateObservationCommand, CommitGroupBatch, CommitGroupOutcome, ComputerEvidenceCommand,
    ComputerEvidenceOutcome, CreatedBranch, CreatedSession, CreatedSessionFork,
    GraphAbandonCommand, GraphAbandonOutcome, GraphEvidenceCommand, GraphEvidenceOutcome,
    GraphFinalizationCommand, GraphFinalizationOutcome, GraphInspectResult, GraphPinCommand,
    GraphPinOutcome, GraphRunSetOpenCommand, GraphRunSetOpenOutcome, GraphSwitchCommand,
    GraphSwitchOutcome, HarnessHandle, MenuResolutionCommand, MenuResolutionOutcome,
    OpenedGraphRunSet, PinnedGraph, ProcessSignalCommand, ProcessSignalOutcome, ProfileStoreFault,
    PromptHistoryCache, ProviderViewAppendOutcome, ProviderViewAppendRequest, QueueConsumeCommand,
    QueueConsumeOutcome, QueuePromoteCommand, QueuePromoteOutcome, QueueRemoveCommand,
    QueueRemoveOutcome, QueueSnapshot, RenamedSession, RunRetryCommand, RunRetryOutcome,
    SeenSession, SelectedAgentType, SelectedEffort, SelectedFast, SelectedModel,
    SessionCreateCommand, SessionCreateOutcome, SessionForkCommand, SessionForkOutcome,
    SessionMetaforkCommit, SessionProjectionCheckpoint, SessionPromptForkCommand,
    SessionRenameCommand, SessionRenameOutcome, SessionSeenCommand, SessionSeenOutcome,
    SessionSelectAgentTypeCommand, SessionSelectAgentTypeOutcome, SessionSelectEffortCommand,
    SessionSelectEffortOutcome, SessionSelectFastCommand, SessionSelectFastOutcome,
    SessionSelectModelCommand, SessionSelectModelOutcome, ShellExecAcceptCommand,
    ShellExecAcceptOutcome, SqliteStoreHandle, StoreHandle, SwitchedGraph, TurnAcceptCommand,
    TurnAcceptOutcome, TurnAdmissionDisposition, TurnCancelCommand, TurnCancelOutcome,
    TurnCancellationStatus, TurnTraceContext, envelopes_contain_terminal, register_turn_trace,
    turn_trace_for_envelopes, turn_trace_ordinal, unregister_turn_trace_for_envelopes,
};
use haider_protocol::EventPayload;
use haider_protocol::agent::AgentManifest;
use haider_protocol::branch::BranchDescriptor;
use haider_protocol::envelope::{RawEnvelope, envelope_weight_bytes};
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::ids::{
    BranchId, CredentialAlias, DeviceId, EventId, GraphId, ItemId, MenuId, RunId, SessionId,
};
use haider_protocol::menu::{
    AnswerVia, EffectRecoveryAction, Menu, MenuAnswer as DurableMenuAnswer, MenuKind, MenuScope,
    effect_recovery_menu,
};
use haider_protocol::state::RunState;
use haider_rpc::{
    ARTIFACT_PUT_MAX_BYTES, AttachMode, AttachState, AttachmentId, CancelStatus, Capability,
    CapabilitySet, CommandId, ERROR_CODE_ALREADY_RESOLVED, ERROR_CODE_ARTIFACT_TOO_LARGE,
    ERROR_CODE_ATTACHMENT_MIME_UNSUPPORTED, ERROR_CODE_ATTACHMENT_NOT_FOUND,
    ERROR_CODE_ATTACHMENT_TOO_LARGE, ERROR_CODE_ATTACHMENTS_TOO_LARGE, ERROR_CODE_BUSY,
    ERROR_CODE_CAPABILITY_DENIED, ERROR_CODE_CURSOR_AHEAD, ERROR_CODE_DRAINING,
    ERROR_CODE_FORK_CUT_UNSTABLE, ERROR_CODE_GRAPH_ALREADY_ACTIVE, ERROR_CODE_GRAPH_NOT_ACTIVE,
    ERROR_CODE_GRAPH_WRONG_NODE, ERROR_CODE_INVALID_ARGUMENT, ERROR_CODE_INVALID_CURSOR,
    ERROR_CODE_NOT_FOUND, ERROR_CODE_OVERLOADED, ERROR_CODE_PDF_MALFORMED,
    ERROR_CODE_PDF_TOO_LARGE, ERROR_CODE_PDF_TOO_MANY_PAGES, ERROR_CODE_PEER_AMBIGUOUS,
    ERROR_CODE_PEER_INVALID, ERROR_CODE_PEER_UNAVAILABLE, ERROR_CODE_PROVIDER_MODELS_UNKNOWN,
    ERROR_CODE_REVISION_CONFLICT, ERROR_CODE_RUN_NOT_ACTIVE, ERROR_CODE_STALE_GENERATION,
    ERROR_CODE_SURFACE_TEXT_TOO_LARGE, ERROR_CODE_TOO_MANY_ATTACHMENTS,
    ERROR_CODE_UNSUPPORTED_SHELL_BUILTIN, ERROR_CODE_VISION_UNSUPPORTED, ErrorData, MenuInput,
    ProtocolError, RequestBody, RequestId, ResponseBody, SURFACE_INPUT_MAX_BYTES,
    SURFACE_STATUS_MAX_BYTES, SeqRange, SessionReadResult, SessionSummary, SubmitDisposition,
    SurfaceInjectOp, SurfaceInputPublishWire, SurfaceInputWire, SurfaceStatusPublishWire,
    SurfaceStatusWire, TodoGraphOpenedWire, WireFrame, WorkflowCatalogEntryV1,
    WorkflowInstanceSourceV1, WorkflowInstanceV1,
};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

#[cfg(test)]
use replay::{FrameDelivery, deliver_frame};
use replay::{ReplayCompletion, run_replay};

const REPLAY_PAGE_SIZE: usize = 256;
const MAX_LIST_PAGE: usize = 100;
const MAX_READ_ENVELOPES: usize = 1_024;
const APPEND_QUEUE_MAX_REQUESTS: usize = 128;
const APPEND_QUEUE_MAX_BYTES: usize = 64 * 1024 * 1024;
const APPEND_GROUP_MAX_REQUESTS: usize = 32;
const APPEND_GROUP_MAX_BYTES: usize = 16 * 1024 * 1024;
/// These channels carry coalescing wakeups, never authoritative state. A
/// lagged receiver repairs from its durable cursor, so 256 entries preserve
/// behavior while bounding two always-live rings at one quarter of the old
/// capacity.
const PUBLICATION_RING_CAPACITY: usize = 256;
/// A tight multi-turn exchange keeps its incremental projections hot. Once
/// the exact idle journal head remains unchanged for five seconds, all state
/// released below is reconstructible from that journal and its checkpoints.
const IDLE_DERIVED_STATE_RELEASE_DELAY: Duration = Duration::from_secs(5);

fn retention_trace_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("HAIDER_DAEMON_RETENTION_TRACE").is_some())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FleetNodeIdentity {
    callsign: Option<String>,
    model: Option<String>,
    provider: Option<String>,
}

/// Projects only durable manifest identity. Both fleet delivery paths call
/// this helper so neither can fill a missing coordinate from parent/session
/// state or drift from the other path.
fn fleet_node_identity(manifest: &AgentManifest) -> FleetNodeIdentity {
    let persisted = |value: &str| (!value.trim().is_empty()).then(|| value.to_owned());
    FleetNodeIdentity {
        callsign: manifest.callsign.as_deref().and_then(persisted),
        model: persisted(&manifest.model_profile),
        provider: manifest.provider().and_then(persisted),
    }
}

/// Real-time home for the UTC-aligned usage-ledger timer.
///
/// Session hubs also run under paused Tokio clocks in deterministic protocol
/// tests. A wall-derived deadline installed on that same clock gives its
/// auto-advance loop a far-future target and can postpone observation of
/// already-ready socket I/O by hundreds of virtual seconds. Keeping this
/// wall-clock service on one process-global real-time runtime preserves the
/// production schedule without coupling unrelated connection clocks to it.
fn usage_history_runtime() -> Result<&'static tokio::runtime::Runtime, SessionHubError> {
    static RUNTIME: OnceLock<Result<tokio::runtime::Runtime, String>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_time()
                .thread_name("haider-usage-history")
                .build()
                .map_err(|error| format!("cannot start usage-history timer runtime: {error}"))
        })
        .as_ref()
        .map_err(|message| SessionHubError::Task(message.clone()))
}

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
                // A newly generated diagnostic key must retain full durability.
                haider_platform::fs::sync_file(&file, haider_platform::SyncPolicy::Full)?;
                // The key's new directory entry shares the same full-durability boundary.
                haider_platform::fs::sync_directory(root, haider_platform::SyncPolicy::Full)?;
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

    /// Admits one frame on a named required FIFO stream. Production sinks
    /// keep this lane outside the priority reply floor so a later cursor seal
    /// cannot overtake an earlier stream record. Sinks without keyed lanes
    /// retain their single-queue behavior.
    fn offer_ordered(&self, _stream_id: &str, frame: &WireFrame) -> SendAdmission {
        match self.try_send(frame.clone()) {
            Ok(()) => SendAdmission::Sent,
            Err(FrameSendError) => SendAdmission::Refused,
        }
    }

    fn offer_ordered_prepared(&self, stream_id: &str, frame: &PreparedFrame) -> SendAdmission {
        frame
            .logical_frame()
            .map_or(SendAdmission::Refused, |frame| {
                self.offer_ordered(stream_id, frame)
            })
    }

    fn offer_ordered_ticketed(
        &self,
        stream_id: &str,
        frame: &WireFrame,
        _ticket: &AdmissionTicket,
    ) -> SendAdmission {
        self.offer_ordered(stream_id, frame)
    }

    fn offer_ordered_prepared_ticketed(
        &self,
        stream_id: &str,
        frame: &PreparedFrame,
        ticket: &AdmissionTicket,
    ) -> SendAdmission {
        frame
            .logical_frame()
            .map_or(SendAdmission::Refused, |frame| {
                self.offer_ordered_ticketed(stream_id, frame, ticket)
            })
    }

    /// Removes not-yet-written frames for a replaced ordered stream.
    fn purge_ordered(&self, _stream_id: &str) {}

    /// Stages a `SessionAttach` or `SessionDescendantsAttach` RESPONSE with
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

/// Test-only process boundary recorder used by the turn durability sweep.
///
/// A `Persisted` observation is emitted synchronously after the store reports
/// a successful transaction and before the actor publishes or starts the next
/// external action. The observer records that boundary durably, then parks the
/// actor at one requested ordinal until the owning test sends SIGKILL. Release
/// builds retain the seam so the installed-binary QA gate can exercise the
/// real daemon, but it is inert unless the explicitly test-named environment
/// variables are present.
struct JournalBoundaryObserver {
    ledger: PathBuf,
    kill_after_ordinal: Option<u64>,
    next_ordinal: Mutex<u64>,
}

impl SessionHubObserver for JournalBoundaryObserver {
    fn observe(&self, observation: HubObservation) {
        let HubObservation::Persisted { through_seq, .. } = observation else {
            return;
        };
        // Allocation, durable row append, and the optional park are one
        // serialized test boundary. Concurrent session actors therefore
        // cannot reorder ordinal rows around the process kill.
        let mut next_ordinal = self
            .next_ordinal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *next_ordinal = next_ordinal.saturating_add(1);
        let ordinal = *next_ordinal;
        let recorded = (|| -> std::io::Result<()> {
            use std::io::Write as _;
            let mut ledger = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.ledger)?;
            writeln!(ledger, "{ordinal}\t{through_seq}")?;
            ledger.sync_data()
        })();
        if let Err(error) = recorded {
            tracing::error!(
                target: "haider.turn",
                operation_micros = 0_u64,
                "test journal-boundary ledger write failed: {error}"
            );
            return;
        }
        if self.kill_after_ordinal == Some(ordinal) {
            loop {
                std::thread::park_timeout(std::time::Duration::from_millis(50));
            }
        }
    }
}

fn configured_session_hub_observer() -> Result<Arc<dyn SessionHubObserver>, SessionHubError> {
    const FILE_ENV: &str = "HAIDER_TEST_JOURNAL_BOUNDARY_FILE";
    const ORDINAL_ENV: &str = "HAIDER_TEST_JOURNAL_KILL_AFTER";
    let Some(ledger) = std::env::var_os(FILE_ENV) else {
        if std::env::var_os(ORDINAL_ENV).is_some() {
            return Err(SessionHubError::Task(format!(
                "{ORDINAL_ENV} requires {FILE_ENV}"
            )));
        }
        return Ok(Arc::new(NoopObserver));
    };
    let ledger = PathBuf::from(ledger);
    if ledger.as_os_str().is_empty() || !ledger.is_absolute() {
        return Err(SessionHubError::Task(format!(
            "{FILE_ENV} must name an absolute test ledger path"
        )));
    }
    let profile = std::env::var_os("HAIDER_PROFILE_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| SessionHubError::Task(format!("{FILE_ENV} requires HAIDER_PROFILE_DIR")))?;
    let profile = std::fs::canonicalize(&profile).map_err(|error| {
        SessionHubError::Task(format!("cannot resolve test profile directory: {error}"))
    })?;
    let parent = ledger
        .parent()
        .ok_or_else(|| SessionHubError::Task(format!("{FILE_ENV} has no parent directory")))?;
    let parent = std::fs::canonicalize(parent).map_err(|error| {
        SessionHubError::Task(format!("cannot resolve test boundary directory: {error}"))
    })?;
    if !parent.starts_with(&profile) {
        return Err(SessionHubError::Task(format!(
            "{FILE_ENV} must stay inside HAIDER_PROFILE_DIR"
        )));
    }
    let kill_after_ordinal = std::env::var(ORDINAL_ENV)
        .ok()
        .map(|value| {
            value.parse::<u64>().map_err(|_| {
                SessionHubError::Task(format!(
                    "{ORDINAL_ENV} must be a positive journal boundary ordinal"
                ))
            })
        })
        .transpose()?;
    if kill_after_ordinal == Some(0) {
        return Err(SessionHubError::Task(format!(
            "{ORDINAL_ENV} must be a positive journal boundary ordinal"
        )));
    }
    Ok(Arc::new(JournalBoundaryObserver {
        ledger,
        kill_after_ordinal,
        next_ordinal: Mutex::new(0),
    }))
}

// ──────────────────── hub state, task ownership, errors ─────────────────────

/// Cloneable owner of every live session actor and replay task.
#[derive(Clone)]
pub struct SessionHub {
    inner: Arc<HubInner>,
}

/// Non-owning hub reference used by daemon-lifetime services. Keeping source
/// listeners weak prevents their tasks from extending the hub lifetime.
#[derive(Clone)]
pub(crate) struct WeakSessionHub {
    inner: Weak<HubInner>,
}

/// Per-session idle fence held while peer delivery moves from the shared
/// mailbox claim into the target daemon's core turn store.
pub(crate) struct PeerTurnClaim {
    session_id: SessionId,
    _selection: tokio::sync::OwnedMutexGuard<()>,
}

impl WeakSessionHub {
    pub(crate) fn upgrade(&self) -> Option<SessionHub> {
        self.inner.upgrade().map(|inner| SessionHub { inner })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LockdownTurnBinding {
    provider: String,
    policy: crate::auto_hermetic::ProviderLockdownPolicy,
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
    /// Child ids whose fork transaction/publication barrier has not completed.
    /// External actor admission and peer rosters exclude them; publication may
    /// install the child's actor behind this fence before making it visible.
    fork_candidates: Mutex<HashSet<SessionId>>,
    /// Fork publication spans source/child journals, SSH scope, Pipe, actor,
    /// and roster state. Serialize that cross-session sequence so a receipt
    /// replay cannot outrun the winning publication barrier.
    fork_publication_serial: tokio::sync::Mutex<()>,
    /// Connection-level unsolicited sinks. Store failures fan out here, and
    /// volatile input injection uses the current publisher's indexed route.
    diagnostic_sinks: Mutex<HashMap<String, Arc<dyn FrameSink>>>,
    /// Connections that explicitly used a peer-messaging method. This is the
    /// feature absence fence: no additive peer frame is sent to an older
    /// connection that never opted into the feature family.
    peer_event_subscribers: Mutex<HashSet<String>>,
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
    /// Per-session actor joins are kept separately so deletion can join and
    /// release the completed task immediately instead of retaining one handle
    /// for every session served until daemon shutdown.
    session_actor_tasks: Mutex<HashMap<SessionId, JoinHandle<()>>>,
    actor_tasks: Mutex<Vec<JoinHandle<()>>>,
    /// Wakes the daemon-wide shell fan-out actor before shutdown joins the
    /// hub-owned task set. The registry sender itself lives in `HubInner`, so
    /// dropping the hub cannot close that actor's broadcast receiver first.
    shell_registry_events_cancel: watch::Sender<bool>,
    /// Serializes every package-manager side effect in this daemon. Durable
    /// CAS protects state, while this process-local mutex prevents a startup
    /// resume and a concurrent registration from both executing the same
    /// already-Installing item before either reaches its post-effect CAS.
    typed_install_serial: Arc<tokio::sync::Mutex<()>>,
    /// Orders native workflow selection against every run admission. RPC pin
    /// and switch hold this through their idle check and commit; turn, retry,
    /// shell, and manual-compaction admission hold it until the nonterminal
    /// run fact is committed. This closes the request TOCTOU window without
    /// blocking daemon-internal workflow_author transitions inside an already
    /// fenced tool call.
    workflow_selection_serials: tokio::sync::Mutex<HashMap<SessionId, Arc<tokio::sync::Mutex<()>>>>,
    /// Weak root-run budget authorities shared by delegated child workers.
    /// The owning parent turn controls their lifetime; stale entries cannot
    /// keep a completed run or worker lease alive.
    run_budget_coordinators:
        Mutex<HashMap<(SessionId, RunId), Weak<crate::worker::RunBudgetCoordinator>>>,
    /// Serializes receipt lookup through workspace publication for checkpoint
    /// commands. This makes same-command retries idempotent even when two
    /// control connections race before either response is visible.
    checkpoint_serials: tokio::sync::Mutex<HashMap<SessionId, Arc<tokio::sync::Mutex<()>>>>,
    replay_tasks: Mutex<Vec<JoinHandle<ReplayCompletion>>>,
    attachments: Mutex<HashMap<AttachmentId, AttachmentOwner>>,
    /// Connection-scoped nested-subagent streams. They share the ordinary
    /// attachment admission ledger and outbox lanes but have no single
    /// session actor because every child journal owns an independent cursor.
    descendant_attachments: Mutex<HashMap<AttachmentId, DescendantAttachmentOwner>>,
    /// Wakes descendant streams after durable lineage-row mutations. Child
    /// journal commits use `roster_publications`; a periodic audit remains
    /// the lag/restart repair backstop for both channels.
    descendant_lineage_publications: watch::Sender<u64>,
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
    peer_service: Mutex<Option<Arc<crate::peer::PeerService>>>,
    #[cfg(test)]
    peer_handoff_count: AtomicU64,
    loom_author_provider: Mutex<Option<Arc<dyn crate::worker::ProviderFactory>>>,
    accounts: Mutex<Option<crate::accounts::AccountsFacade>>,
    /// Installed beside the profile-scoped, owner-only secret vault. SSH sessions are
    /// process-local and shared by every Haider session in this profile.
    ssh: Mutex<Option<crate::ssh::SshService>>,
    /// Session launch/interactive scope. Absence means the v1 default: All.
    ssh_scopes: Mutex<HashMap<SessionId, crate::ssh::SshScope>>,
    /// A setter whose durable write and retry both failed is crash-ambiguous.
    /// Forks refuse that source until a later successful setter re-establishes
    /// one cache/vault authority.
    ssh_scope_uncertain: Mutex<HashSet<SessionId>>,
    /// One registry for all daemon-owned terminal channels.
    shells: crate::shell_registry::ShellRegistry,
    creatable_providers: Mutex<Option<std::collections::BTreeSet<String>>>,
    hooks: Arc<Mutex<Option<crate::hooks::WeakHookService>>>,
    /// One post-commit fan-out shared by every session actor. The journal is
    /// still authoritative: the observe fold rebuilds on miss/gap, while the
    /// roster channel is only a coalescing wake carrying the dirty session.
    commit_projection: Arc<CommitProjection>,
    observe_digests: Arc<rpc::ObserveDigestCache>,
    roster_publications: broadcast::Sender<SessionId>,
    /// Coalescing wake after a committed Loom registry event. Watchers always
    /// repair from the durable cursor log; this channel is never authority.
    loom_registry_publications: broadcast::Sender<u64>,
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
    /// §E: source subscriptions and the in-memory projection of durable
    /// monitor journal facts. The projection is rebuilt at startup.
    monitors: crate::monitor::MonitorService,
    /// W-B: session-scoped web-capability degrades (anthropic server tools
    /// 400ed → local fallback; codex alpha/search 404/410 → stop advertising
    /// the client search). Deliberately IN-MEMORY: "for the session" is a
    /// runtime scope — a daemon restart retries the capability once.
    web_degrade: Mutex<HashMap<SessionId, crate::worker::WebCapabilityDegrade>>,
    /// Daemon-authoritative provider ceiling frozen from the account adapter
    /// the worker actually resolved. Hooks can observe the committed
    /// acceptance first, but wait on `lockdown_turn_bound` instead of deriving
    /// authority from a mutable management snapshot.
    lockdown_turns: Mutex<HashMap<(SessionId, RunId), LockdownTurnBinding>>,
    /// Wakes hook dispatch after the worker publishes the resolved-account
    /// ceiling. `Notify` is paired with a check-before-wait loop so a binding
    /// committed just before subscription cannot be missed.
    lockdown_turn_bound: Notify,
    /// Ephemeral compiled-prompt acceleration. Journal bytes remain the
    /// authority; the cache is discarded with this daemon generation.
    prompt_history: PromptHistoryCache,
    /// Exact durable-head prefixes for the worker's fused turn-setup fold.
    /// Like prompt history, this is daemon-lifetime only; restart rebuilds
    /// from journal authority before installing a new revision.
    turn_setup_reductions: TurnSetupReductionCache,
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

struct ForkCandidateReservation {
    inner: Arc<HubInner>,
    session_id: SessionId,
    committed: bool,
}

impl Drop for ForkCandidateReservation {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Ok(mut candidates) = self.inner.fork_candidates.lock() {
            candidates.remove(&self.session_id);
        }
    }
}

impl ForkCandidateReservation {
    fn retain_until_published(&mut self) {
        self.committed = true;
    }
}

enum SessionForkRequest {
    Exact(SessionForkCommand),
    Prompt(SessionPromptForkCommand),
}

#[derive(Clone, Copy)]
enum AppendCommitKind {
    General,
    Worker,
}

enum AppendCommitCompletion {
    Append(oneshot::Sender<Result<Vec<RawEnvelope>, HaiderError>>),
    CreateSession(oneshot::Sender<Result<SessionCreateOutcome, HaiderError>>),
    AcceptTurn(oneshot::Sender<Result<TurnAcceptOutcome, HaiderError>>),
    HookAcks(oneshot::Sender<Result<(), HaiderError>>),
}

struct PendingCommitCompletion {
    completed: AppendCommitCompletion,
    _byte_permit: OwnedSemaphorePermit,
}

impl PendingCommitCompletion {
    fn send(self, outcome: Result<CommitGroupOutcome, HaiderError>) {
        match (self.completed, outcome) {
            (AppendCommitCompletion::Append(completed), Ok(CommitGroupOutcome::Append(value))) => {
                let _ = completed.send(Ok(value));
            }
            (
                AppendCommitCompletion::CreateSession(completed),
                Ok(CommitGroupOutcome::CreateSession(value)),
            ) => {
                let _ = completed.send(Ok(value));
            }
            (
                AppendCommitCompletion::AcceptTurn(completed),
                Ok(CommitGroupOutcome::AcceptTurn(value)),
            ) => {
                let _ = completed.send(Ok(value));
            }
            (AppendCommitCompletion::HookAcks(completed), Ok(CommitGroupOutcome::HookAcks)) => {
                let _ = completed.send(Ok(()));
            }
            (AppendCommitCompletion::Append(completed), Err(error)) => {
                let _ = completed.send(Err(error));
            }
            (AppendCommitCompletion::CreateSession(completed), Err(error)) => {
                let _ = completed.send(Err(error));
            }
            (AppendCommitCompletion::AcceptTurn(completed), Err(error)) => {
                let _ = completed.send(Err(error));
            }
            (AppendCommitCompletion::HookAcks(completed), Err(error)) => {
                let _ = completed.send(Err(error));
            }
            (completed, Ok(_)) => {
                let error = commit_group_shape_error();
                match completed {
                    AppendCommitCompletion::Append(completed) => {
                        let _ = completed.send(Err(error));
                    }
                    AppendCommitCompletion::CreateSession(completed) => {
                        let _ = completed.send(Err(error));
                    }
                    AppendCommitCompletion::AcceptTurn(completed) => {
                        let _ = completed.send(Err(error));
                    }
                    AppendCommitCompletion::HookAcks(completed) => {
                        let _ = completed.send(Err(error));
                    }
                }
            }
        }
    }
}

struct AppendCommitRequest {
    batch: CommitGroupBatch,
    byte_weight: usize,
    _byte_permit: OwnedSemaphorePermit,
    completed: AppendCommitCompletion,
}

impl AppendCommitRequest {
    fn into_parts(self) -> (CommitGroupBatch, PendingCommitCompletion) {
        (
            self.batch,
            PendingCommitCompletion {
                completed: self.completed,
                _byte_permit: self._byte_permit,
            },
        )
    }
}

enum AppendCommitMessage {
    Commit(AppendCommitRequest),
    Shutdown(oneshot::Sender<()>),
}

/// Profile-global group-commit admission shared by every session actor.
#[derive(Clone)]
struct AppendCommitter {
    requests: mpsc::Sender<AppendCommitMessage>,
    bytes: Arc<Semaphore>,
    admitted: Arc<AtomicUsize>,
}

impl AppendCommitter {
    fn has_admitted_mutation(&self) -> bool {
        self.admitted.load(Ordering::Acquire) > 0
    }

    async fn admit(&self, byte_weight: usize) -> Result<OwnedSemaphorePermit, HaiderError> {
        // A request is indivisible. Charging an oversized request the entire
        // semaphore admits it alone while its existing producer-side limit
        // remains the absolute single-request ceiling.
        let charged_bytes = byte_weight.clamp(1, APPEND_QUEUE_MAX_BYTES);
        let permits = u32::try_from(charged_bytes).map_err(|_| {
            HaiderError::new(
                ErrorCode::Internal,
                "append admission byte charge exceeds semaphore range",
                false,
            )
        })?;
        Arc::clone(&self.bytes)
            .acquire_many_owned(permits)
            .await
            .map_err(|_| hub_closed_store_error())
    }

    async fn send(
        &self,
        batch: CommitGroupBatch,
        byte_weight: usize,
        completed: AppendCommitCompletion,
    ) -> Result<(), HaiderError> {
        let byte_permit = self.admit(byte_weight).await?;
        self.admitted.fetch_add(1, Ordering::AcqRel);
        let result = self
            .requests
            .send(AppendCommitMessage::Commit(AppendCommitRequest {
                batch,
                byte_weight,
                _byte_permit: byte_permit,
                completed,
            }))
            .await;
        if result.is_err() {
            self.admitted.fetch_sub(1, Ordering::AcqRel);
            return Err(hub_closed_store_error());
        }
        Ok(())
    }

    async fn commit(
        &self,
        kind: AppendCommitKind,
        envelopes: Vec<RawEnvelope>,
    ) -> Result<Vec<RawEnvelope>, HaiderError> {
        let byte_weight = envelopes
            .iter()
            .map(envelope_weight_bytes)
            .fold(0_usize, usize::saturating_add);
        let (completed, result) = oneshot::channel();
        self.send(
            CommitGroupBatch::Append(AppendGroupBatch {
                envelopes,
                validate_worker_transitions: matches!(kind, AppendCommitKind::Worker),
            }),
            byte_weight,
            AppendCommitCompletion::Append(completed),
        )
        .await?;
        result.await.map_err(|_| hub_closed_store_error())?
    }

    async fn accept_turn(
        &self,
        command: TurnAcceptCommand,
        peer_message: Option<haider_protocol::peer::PeerMessage>,
        auto_title: Option<String>,
    ) -> Result<TurnAcceptOutcome, HaiderError> {
        let byte_weight = command
            .request_json
            .len()
            .saturating_add(command.text.len())
            .saturating_add(std::mem::size_of::<TurnAcceptCommand>());
        let validate_headless = peer_message.is_none() && auto_title.is_some();
        let (completed, result) = oneshot::channel();
        self.send(
            CommitGroupBatch::AcceptTurn {
                command,
                peer_message,
                auto_title,
                validate_headless,
            },
            byte_weight,
            AppendCommitCompletion::AcceptTurn(completed),
        )
        .await?;
        result.await.map_err(|_| hub_closed_store_error())?
    }

    async fn create_session(
        &self,
        command: SessionCreateCommand,
        interaction_mode: haider_protocol::session::SessionInteractionModeV1,
        account_alias: Option<String>,
    ) -> Result<SessionCreateOutcome, HaiderError> {
        let byte_weight = command
            .request_json
            .len()
            .saturating_add(command.cwd.len())
            .saturating_add(command.provider.len())
            .saturating_add(command.model.len())
            .saturating_add(std::mem::size_of::<SessionCreateCommand>());
        let (completed, result) = oneshot::channel();
        self.send(
            CommitGroupBatch::CreateSession {
                command,
                interaction_mode,
                account_alias,
            },
            byte_weight,
            AppendCommitCompletion::CreateSession(completed),
        )
        .await?;
        result.await.map_err(|_| hub_closed_store_error())?
    }

    async fn complete_hook_dispatches(
        &self,
        acks: Vec<(SessionId, u64)>,
    ) -> Result<(), HaiderError> {
        if acks.is_empty() {
            return Ok(());
        }
        let byte_weight = acks.iter().fold(0_usize, |weight, (session_id, _)| {
            weight
                .saturating_add(session_id.as_str().len())
                .saturating_add(std::mem::size_of::<u64>())
        });
        let (completed, result) = oneshot::channel();
        self.send(
            CommitGroupBatch::HookAcks(acks),
            byte_weight,
            AppendCommitCompletion::HookAcks(completed),
        )
        .await?;
        result.await.map_err(|_| hub_closed_store_error())?
    }

    async fn shutdown(&self) {
        let (completed, result) = oneshot::channel();
        if self
            .requests
            .send(AppendCommitMessage::Shutdown(completed))
            .await
            .is_ok()
        {
            let _ = result.await;
        }
    }
}

async fn run_append_committer(
    store: SqliteStoreHandle,
    mut requests: mpsc::Receiver<AppendCommitMessage>,
    admitted: Arc<AtomicUsize>,
) {
    let mut next_message = None;
    loop {
        let message = match next_message.take() {
            Some(message) => message,
            None => match requests.recv().await {
                Some(message) => message,
                None => break,
            },
        };
        let first = match message {
            AppendCommitMessage::Commit(first) => first,
            AppendCommitMessage::Shutdown(completed) => {
                let _ = completed.send(());
                break;
            }
        };
        let mut pending_bytes = first.byte_weight;
        let mut pending = vec![first];
        let mut shutdown = None;
        while pending.len() < APPEND_GROUP_MAX_REQUESTS {
            let Ok(message) = requests.try_recv() else {
                break;
            };
            if !push_append_commit_message(
                message,
                &mut pending,
                &mut pending_bytes,
                &mut next_message,
                &mut shutdown,
            ) {
                break;
            }
        }

        let (batches, completions): (Vec<_>, Vec<_>) = pending
            .into_iter()
            .map(AppendCommitRequest::into_parts)
            .unzip();
        let completed_count = completions.len();
        match store.commit_group(batches).await {
            Ok(outcomes) => {
                for (completed, outcome) in completions.into_iter().zip(outcomes) {
                    completed.send(outcome);
                }
            }
            Err(error) => {
                for completed in completions {
                    completed.send(Err(error.clone()));
                }
            }
        }
        admitted.fetch_sub(completed_count, Ordering::AcqRel);
        if let Some(completed) = shutdown {
            let _ = completed.send(());
            break;
        }
    }
}

fn push_append_commit_message(
    message: AppendCommitMessage,
    pending: &mut Vec<AppendCommitRequest>,
    pending_bytes: &mut usize,
    next_message: &mut Option<AppendCommitMessage>,
    shutdown: &mut Option<oneshot::Sender<()>>,
) -> bool {
    match message {
        AppendCommitMessage::Commit(request)
            if pending_bytes.saturating_add(request.byte_weight) > APPEND_GROUP_MAX_BYTES =>
        {
            *next_message = Some(AppendCommitMessage::Commit(request));
            false
        }
        AppendCommitMessage::Commit(request) => {
            *pending_bytes = pending_bytes.saturating_add(request.byte_weight);
            pending.push(request);
            true
        }
        AppendCommitMessage::Shutdown(completed) => {
            *shutdown = Some(completed);
            false
        }
    }
}

fn commit_group_shape_error() -> HaiderError {
    HaiderError::new(
        ErrorCode::Internal,
        "profile committer returned a mismatched request outcome",
        false,
    )
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

/// Ownership for one `session.descendants.attach` stream.
struct DescendantAttachmentOwner {
    connection_id: String,
    session_ids: HashSet<SessionId>,
    cancel: watch::Sender<bool>,
}

/// One committed envelope in flight on a catch-up channel. All entries for a
/// shared committed batch index the same allocation; the final queued entry
/// carries that allocation's full charge so receive-side credit happens only
/// after no earlier entry can retain it.
struct QueuedEnvelope {
    weight: usize,
    envelopes: Arc<[RawEnvelope]>,
    index: usize,
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

enum DescendantRegisterResult {
    Registered {
        attachment_id: AttachmentId,
        cancel: watch::Receiver<bool>,
    },
    Overloaded {
        message: String,
    },
    SessionUnavailable,
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
        interaction_mode: haider_protocol::session::SessionInteractionModeV1,
        account_alias: Option<String>,
        completed: oneshot::Sender<Result<SessionCreateOutcome, HaiderError>>,
    },
    CreateBranch {
        command: BranchCreateCommand,
        completed: oneshot::Sender<Result<BranchCreateOutcome, HaiderError>>,
    },
    CommitCheckpoint {
        command: CheckpointCommitCommand,
        completed: oneshot::Sender<Result<CheckpointCommitOutcome, HaiderError>>,
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
        expected_digest: Option<String>,
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
        peer_message: Option<haider_protocol::peer::PeerMessage>,
        auto_title: Option<String>,
        completed: oneshot::Sender<Result<TurnAcceptOutcome, HaiderError>>,
    },
    QueueList {
        completed: oneshot::Sender<Result<QueueSnapshot, HaiderError>>,
    },
    QueueRemove {
        command: QueueRemoveCommand,
        completed: oneshot::Sender<Result<QueueRemoveOutcome, HaiderError>>,
    },
    QueuePromoteSteer {
        command: QueuePromoteCommand,
        completed: oneshot::Sender<Result<(QueuePromoteOutcome, bool), HaiderError>>,
    },
    QueueConsume {
        lease_id: WorkerLeaseId,
        command: QueueConsumeCommand,
        completed: oneshot::Sender<Result<Option<QueueConsumeOutcome>, HaiderError>>,
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
    WorkerProviderViewAppend {
        lease_id: WorkerLeaseId,
        request: ProviderViewAppendRequest,
        completed: oneshot::Sender<Result<ProviderViewAppendOutcome, HaiderError>>,
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
        completed: oneshot::Sender<()>,
    },
    FenceIfQuiescent {
        completed: oneshot::Sender<Result<bool, HaiderError>>,
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
    /// Listener and PID-publication paths carried by the negotiated runtime
    /// connection. Test-only in-memory connections intentionally omit them.
    runtime_paths: Option<(std::path::PathBuf, std::path::PathBuf)>,
    daemon_idle_ttl_ms: Option<u64>,
    daemon_warm: bool,
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
    /// One required-delivery, cursor-replayable monitor stream. A new
    /// `monitor.watch` replaces the prior registration on this connection.
    monitor_watch: Mutex<Option<MonitorWatchState>>,
    /// One required-delivery Loom registry stream per connection.
    loom_registry_watch: Mutex<Option<LoomRegistryWatchState>>,
    /// Serializes attach/replacement setup while the synchronous state slot
    /// remains available to close/drop for cancellation.
    loom_registry_watch_serial: tokio::sync::Mutex<()>,
    /// Write-free metafork reviews awaiting an explicit acceptance on this
    /// connection. Shared with command-capture facades; dropped on disconnect.
    metafork_reviews: Arc<Mutex<HashMap<String, String>>>,
    /// Ephemeral editable Loom drafts issued to this connection. Sharing the
    /// map with response-capture facades preserves ownership while dropping
    /// the connection discards every authoring session.
    loom_author_sessions: Arc<Mutex<HashMap<String, crate::loom_author::LoomAuthorSession>>>,
    /// The transport-created identity lease. Response-capture facades share
    /// this lease, so they cannot independently own (and therefore cannot
    /// independently tear down) the caller's live identity.
    identity_lease: Arc<ConnectionIdentityLease>,
    closed: AtomicBool,
}

struct MonitorWatchState {
    stream_id: String,
    cancel: watch::Sender<bool>,
    task: JoinHandle<()>,
}

struct LoomRegistryWatchState {
    watch_id: String,
    cancel: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}

/// RAII ownership of an identity registered by `open_connection`.
///
/// Keeping teardown on the last shared lease, rather than on every
/// `HubConnection`-shaped view, makes borrowed command facades structurally
/// incapable of unregistering the identity they only use for authorization.
struct ConnectionIdentityLease {
    hub: SessionHub,
    connection_id: String,
    loom_author_cancel: watch::Sender<bool>,
}

impl Drop for ConnectionIdentityLease {
    fn drop(&mut self) {
        self.loom_author_cancel.send_replace(true);
        self.hub.clear_resident_binding(&self.connection_id);
        let _ = self.hub.inner.shells.close_owner(&self.connection_id);
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
        if let Ok(watch) = self.monitor_watch.get_mut()
            && let Some(watch) = watch.take()
        {
            watch.cancel.send_replace(true);
            self.sink.purge_ordered(&watch.stream_id);
        }
        if let Ok(watch) = self.loom_registry_watch.get_mut()
            && let Some(watch) = watch.take()
        {
            watch.cancel.send_replace(true);
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

#[derive(Debug)]
enum CheckpointCommitFailure {
    /// The command never reached a committing store transaction, or that
    /// transaction returned an error under the store's rollback guarantee.
    DefinitelyUncommitted(SessionHubError),
    /// The actor accepted the command but its acknowledgement channel closed;
    /// the transaction may already be durable, so compensating disk writes
    /// would risk diverging the workspace from the journal.
    Ambiguous(SessionHubError),
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

fn fork_pipe_publication_error(error: crate::pipe_native::PipeNativeError) -> SessionHubError {
    match error.into_store_error() {
        Ok(error) => SessionHubError::Store(error),
        Err(error) => SessionHubError::Task(format!(
            "native Pipe fork projection did not reach the committed head: {error}"
        )),
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
    async fn trace_retention_snapshot(&self, session_id: &SessionId, head_seq: u64, phase: &str) {
        if !retention_trace_enabled() {
            return;
        }
        let prompt = self.inner.prompt_history.retention_stats().await;
        let turn_setup_entries = self
            .inner
            .turn_setup_reductions
            .retention_entry_count()
            .await;
        let (
            observe_ready,
            observe_building,
            observe_bytes,
            observe_session_runs,
            observe_session_bytes,
        ) = self.inner.observe_digests.retention_stats(session_id);
        let attachments = self
            .inner
            .attachments
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|owner| owner.session_id == *session_id)
            .count();
        eprintln!(
            "haider_retention {}",
            serde_json::json!({
                "phase": phase,
                "session_id": session_id.to_string(),
                "head_seq": head_seq,
                "prompt": prompt,
                "turn_setup_entries": turn_setup_entries,
                "observe_ready": observe_ready,
                "observe_building": observe_building,
                "observe_bytes": observe_bytes,
                "observe_session_runs": observe_session_runs,
                "observe_session_bytes": observe_session_bytes,
                "attachments": attachments,
            })
        );
    }

    fn schedule_idle_derived_state_release(&self, session_id: SessionId, idle_seq: u64) {
        let weak = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            tokio::time::sleep(IDLE_DERIVED_STATE_RELEASE_DELAY).await;
            let Some(inner) = weak.upgrade() else {
                return;
            };
            if !matches!(inner.store.latest_seq(&session_id).await, Ok(head) if head == idle_seq) {
                return;
            }

            let prompt_bytes = inner.prompt_history.evict_session_bodies(&session_id).await;
            let turn_setup_entries = inner
                .turn_setup_reductions
                .remove_session(&session_id)
                .await;
            let observe_bytes = inner
                .observe_digests
                .remove_ready_at_head(&session_id, idle_seq);
            if let Err(error) = inner.store.release_memory().await {
                tracing::debug!(
                    session_id = %session_id,
                    ?error,
                    "idle SQLite memory release failed"
                );
            }
            let allocator_bytes = haider_platform::allocator_pressure_relief();
            if retention_trace_enabled() {
                let hub = SessionHub {
                    inner: Arc::clone(&inner),
                };
                hub.trace_retention_snapshot(&session_id, idle_seq, "released")
                    .await;
            }
            tracing::debug!(
                session_id = %session_id,
                idle_seq,
                prompt_bytes,
                turn_setup_entries,
                observe_bytes,
                allocator_bytes,
                "released journal-reconstructible idle session state"
            );
        });
    }

    /// Creates a hub with production's no-op boundary observer.
    pub fn new(
        store: SqliteStoreHandle,
        config: SessionHubConfig,
    ) -> Result<Self, SessionHubError> {
        Self::with_observer(store, config, Arc::new(NoopObserver))
    }

    /// Builds the live hub around the exact native-pipe writer used by the
    /// shared pre-Ready journal fold.
    pub(crate) fn new_with_pipe_native(
        store: SqliteStoreHandle,
        config: SessionHubConfig,
        pipe_native: Arc<crate::pipe_native::PipeNativeWriter>,
    ) -> Result<Self, SessionHubError> {
        Self::with_observer_and_pipe_native(
            store,
            config,
            configured_session_hub_observer()?,
            pipe_native,
        )
    }

    /// Creates a hub with a semantic-boundary observer.
    pub fn with_observer(
        store: SqliteStoreHandle,
        config: SessionHubConfig,
        observer: Arc<dyn SessionHubObserver>,
    ) -> Result<Self, SessionHubError> {
        let pipe_native = Arc::new(crate::pipe_native::PipeNativeWriter::new(store.root()));
        Self::with_observer_and_pipe_native(store, config, observer, pipe_native)
    }

    fn with_observer_and_pipe_native(
        store: SqliteStoreHandle,
        config: SessionHubConfig,
        observer: Arc<dyn SessionHubObserver>,
        pipe_native: Arc<crate::pipe_native::PipeNativeWriter>,
    ) -> Result<Self, SessionHubError> {
        config.validate().map_err(SessionHubError::InvalidConfig)?;
        let cache_diagnostic_key =
            load_or_create_cache_diagnostic_key(store.root()).map_err(|error| {
                SessionHubError::Task(format!("cannot load cache diagnostic key: {error}"))
            })?;
        let device_id = DeviceId::new(format!("daemon-session-hub-{}", store.worker_generation()));
        let (append_requests, append_receiver) = mpsc::channel(APPEND_QUEUE_MAX_REQUESTS);
        let admitted_commits = Arc::new(AtomicUsize::new(0));
        let append_committer = AppendCommitter {
            requests: append_requests,
            bytes: Arc::new(Semaphore::new(APPEND_QUEUE_MAX_BYTES)),
            admitted: Arc::clone(&admitted_commits),
        };
        let append_commit_task = tokio::spawn(run_append_committer(
            store.clone(),
            append_receiver,
            admitted_commits,
        ));
        let (surface_publications, _) = watch::channel(0_u64);
        let (roster_publications, _) = broadcast::channel(PUBLICATION_RING_CAPACITY);
        let (loom_registry_publications, _) = broadcast::channel(PUBLICATION_RING_CAPACITY);
        let (descendant_lineage_publications, _) = watch::channel(0_u64);
        let (haider_code_plan_changes, _) = watch::channel(0_u64);
        let (shell_registry_events_cancel, _) = watch::channel(false);
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
            fork_candidates: Mutex::new(HashSet::new()),
            fork_publication_serial: tokio::sync::Mutex::new(()),
            diagnostic_sinks: Mutex::new(HashMap::new()),
            peer_event_subscribers: Mutex::new(HashSet::new()),
            resident_binding: Mutex::new(ResidentBindingRegistry::default()),
            resident_binding_viewers: Mutex::new(HashSet::new()),
            surfaces: Mutex::new(HashMap::new()),
            surface_publications,
            deleting_sessions: Mutex::new(HashSet::new()),
            session_actor_tasks: Mutex::new(HashMap::new()),
            actor_tasks: Mutex::new(Vec::new()),
            shell_registry_events_cancel,
            typed_install_serial: Arc::new(tokio::sync::Mutex::new(())),
            workflow_selection_serials: tokio::sync::Mutex::new(HashMap::new()),
            run_budget_coordinators: Mutex::new(HashMap::new()),
            checkpoint_serials: tokio::sync::Mutex::new(HashMap::new()),
            replay_tasks: Mutex::new(Vec::new()),
            attachments: Mutex::new(HashMap::new()),
            descendant_attachments: Mutex::new(HashMap::new()),
            descendant_lineage_publications,
            attachment_slots: Mutex::new(AttachmentSlots::default()),
            draining: AtomicBool::new(false),
            force_stop: Arc::new(AtomicBool::new(false)),
            device_id,
            cache_diagnostic_key,
            worker_manager: Mutex::new(None),
            peer_service: Mutex::new(None),
            #[cfg(test)]
            peer_handoff_count: AtomicU64::new(0),
            loom_author_provider: Mutex::new(None),
            accounts: Mutex::new(None),
            ssh: Mutex::new(None),
            ssh_scopes: Mutex::new(HashMap::new()),
            ssh_scope_uncertain: Mutex::new(HashSet::new()),
            shells: crate::shell_registry::ShellRegistry::default(),
            creatable_providers: Mutex::new(None),
            hooks,
            commit_projection,
            observe_digests,
            roster_publications,
            loom_registry_publications,
            haider_code_plan_changes,
            usage_report: Mutex::new(None),
            tasks: crate::tasks::TaskRegistry::default(),
            monitors: crate::monitor::MonitorService::default(),
            web_degrade: Mutex::new(HashMap::new()),
            lockdown_turns: Mutex::new(HashMap::new()),
            lockdown_turn_bound: Notify::new(),
            prompt_history: PromptHistoryCache::default(),
            turn_setup_reductions: TurnSetupReductionCache::default(),
        });
        let hub = Self { inner };
        hub.spawn_shell_registry_events()?;
        hub.spawn_profile_fault_watcher();
        hub.spawn_usage_history_reconciler()?;
        hub.spawn_typed_install_resume()?;
        Ok(hub)
    }

    fn spawn_shell_registry_events(&self) -> Result<(), SessionHubError> {
        let mut events = self.inner.shells.subscribe();
        let mut cancel = self.inner.shell_registry_events_cancel.subscribe();
        let weak = Arc::downgrade(&self.inner);
        lock(&self.inner.actor_tasks)?.push(tokio::spawn(async move {
            loop {
                let event = match tokio::select! {
                    _ = cancel.changed() => break,
                    event = events.recv() => event,
                } {
                    Ok(event) => event,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let Some(inner) = weak.upgrade() else {
                            break;
                        };
                        let connections = match inner.diagnostic_sinks.lock() {
                            Ok(sinks) => sinks
                                .iter()
                                .map(|(owner, sink)| (owner.clone(), Arc::clone(sink)))
                                .collect::<Vec<_>>(),
                            Err(_) => break,
                        };
                        // Terminal bytes are ordered protocol data, never
                        // best-effort diagnostics. If the bounded registry
                        // fanout overruns, close every affected transport and
                        // PTY instead of continuing with a corrupt terminal.
                        for (owner, sink) in connections {
                            sink.close_after_required_delivery_failure();
                            let _ = inner.shells.close_owner(&owner);
                        }
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                let Some(inner) = weak.upgrade() else {
                    break;
                };
                let (frame, delivery_required) = match event {
                    crate::shell_registry::ShellRegistryEvent::Opened(shell) => {
                        (WireFrame::ShellOpened { shell }, false)
                    }
                    crate::shell_registry::ShellRegistryEvent::State(shell) => {
                        let delivery_required = matches!(
                            &shell.status,
                            haider_rpc::ShellStatusWire::Exited { .. }
                                | haider_rpc::ShellStatusWire::Closed
                        );
                        (WireFrame::ShellState { shell }, delivery_required)
                    }
                    crate::shell_registry::ShellRegistryEvent::Closed(shell) => {
                        (WireFrame::ShellClosed { shell }, true)
                    }
                    crate::shell_registry::ShellRegistryEvent::Output {
                        owner,
                        id,
                        stream,
                        bytes,
                    } => {
                        let frame = WireFrame::ShellOutput {
                            id,
                            stream,
                            chunk_b64: haider_rpc::TerminalOutputWire::new(
                                base64::engine::general_purpose::STANDARD.encode(bytes.as_slice()),
                            ),
                        };
                        let sinks = match inner.diagnostic_sinks.lock() {
                            Ok(sinks) => match owner.as_deref() {
                                Some(owner) => {
                                    sinks.get(owner).cloned().into_iter().collect::<Vec<_>>()
                                }
                                None => sinks.values().cloned().collect::<Vec<_>>(),
                            },
                            Err(_) => break,
                        };
                        let mut refused = false;
                        for sink in sinks {
                            if sink.try_send_droppable(frame.clone()).is_err() {
                                sink.close_after_required_delivery_failure();
                                refused = true;
                            }
                        }
                        if refused && let Some(owner) = owner {
                            let _ = inner.shells.close_owner(&owner);
                        }
                        continue;
                    }
                };
                let sinks = match inner.diagnostic_sinks.lock() {
                    Ok(sinks) => sinks.values().cloned().collect::<Vec<_>>(),
                    Err(_) => break,
                };
                for sink in sinks {
                    if sink.try_send_droppable(frame.clone()).is_err() && delivery_required {
                        sink.close_after_required_delivery_failure();
                    }
                }
            }
        }));
        Ok(())
    }

    fn spawn_typed_install_resume(&self) -> Result<(), SessionHubError> {
        let store = self.inner.store.clone();
        let serial = Arc::clone(&self.inner.typed_install_serial);
        lock(&self.inner.actor_tasks)?.push(tokio::spawn(async move {
            let _exclusive = serial.lock().await;
            crate::typed_agent_runtime::resume_pending_installs(store).await;
        }));
        Ok(())
    }

    fn spawn_typed_install_job(&self, job_id: String) -> Result<(), HaiderError> {
        let store = self.inner.store.clone();
        let mut tasks = self.inner.actor_tasks.lock().map_err(|_| {
            HaiderError::new(
                ErrorCode::Internal,
                "typed-agent installer task registry is unavailable",
                true,
            )
        })?;
        // Keep this check under the same task-registry lock that shutdown
        // takes after publishing the drain flag. A committed job may remain
        // queued for next-start adoption, but no installer task can detach
        // behind the shutdown barrier.
        if self.inner.draining.load(Ordering::Acquire) {
            return Ok(());
        }
        let serial = Arc::clone(&self.inner.typed_install_serial);
        tasks.push(tokio::spawn(async move {
            let _exclusive = serial.lock().await;
            if let Err(error) =
                crate::typed_agent_runtime::run_install_job(store, job_id.clone()).await
            {
                tracing::warn!(job_id = %job_id, %error, "typed-agent install job stopped");
            }
        }));
        Ok(())
    }

    fn spawn_usage_history_reconciler(&self) -> Result<(), SessionHubError> {
        let weak = Arc::downgrade(&self.inner);
        usage_history_runtime()?.spawn(async move {
            const PERIOD_MS: u64 = 15 * 60 * 1_000;
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .and_then(|duration| u64::try_from(duration.as_millis()).ok())
                .unwrap_or(0);
            let until_boundary_ms = PERIOD_MS - (now_ms % PERIOD_MS);
            let start =
                tokio::time::Instant::now() + std::time::Duration::from_millis(until_boundary_ms);
            let mut ticker =
                tokio::time::interval_at(start, std::time::Duration::from_millis(PERIOD_MS));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                let Some(inner) = weak.upgrade() else {
                    break;
                };
                if let Err(error) = inner.store.reconcile_usage_history().await {
                    tracing::warn!(?error, "usage-history reconciliation failed");
                }
            }
        });
        Ok(())
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

    pub(crate) fn inner_monitor(&self) -> &crate::monitor::MonitorService {
        &self.inner.monitors
    }

    pub(crate) fn downgrade(&self) -> WeakSessionHub {
        WeakSessionHub {
            inner: Arc::downgrade(&self.inner),
        }
    }

    /// Source-subscription seam for transport wiring. The returned hub is
    /// instance-scoped and contains no session-selection authority.
    pub fn monitor_source_hub(&self) -> crate::monitor::MonitorSourceHub {
        self.inner.monitors.source_hub()
    }

    pub(crate) async fn wait_for_monitor_ready(&self) {
        self.inner.monitors.wait_ready().await;
    }

    /// Installs a transport-specific report mirror. The default sink remains
    /// authoritative and routes every report through an ordinary durable
    /// turn, so transport wiring cannot accidentally suppress agent wake.
    pub fn install_monitor_delivery_sink(
        &self,
        sink: Arc<dyn crate::monitor::MonitorDeliverySink>,
    ) {
        self.inner.monitors.install_delivery_sink(sink);
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

    pub(crate) fn register_run_budget_coordinator(
        &self,
        session_id: SessionId,
        run_id: RunId,
        coordinator: Arc<crate::worker::RunBudgetCoordinator>,
    ) -> Result<Arc<crate::worker::RunBudgetCoordinator>, SessionHubError> {
        let mut coordinators = lock(&self.inner.run_budget_coordinators)?;
        let key = (session_id, run_id);
        if let Some(existing) = coordinators.get(&key).and_then(Weak::upgrade) {
            return Ok(existing);
        }
        coordinators.insert(key, Arc::downgrade(&coordinator));
        Ok(coordinator)
    }

    pub(crate) fn run_budget_coordinator(
        &self,
        session_id: &SessionId,
        run_id: &RunId,
    ) -> Result<Option<Arc<crate::worker::RunBudgetCoordinator>>, SessionHubError> {
        let mut coordinators = lock(&self.inner.run_budget_coordinators)?;
        let key = (session_id.clone(), run_id.clone());
        let coordinator = coordinators.get(&key).and_then(Weak::upgrade);
        if coordinator.is_none() {
            coordinators.remove(&key);
        }
        Ok(coordinator)
    }

    /// Returns the live provider policy used only when opening a new run
    /// boundary. Missing account infrastructure is retained as Full for
    /// isolated hub tests; once the account facade is installed, an unknown
    /// provider fails closed as lockdown. The automatic branch is evaluated
    /// only for this active provider, never by scanning unrelated profiles.
    pub(crate) fn provider_lockdown_policy_detail(
        &self,
        provider: &str,
    ) -> Result<crate::auto_hermetic::ProviderLockdownPolicy, SessionHubError> {
        let Some(accounts) = self.accounts()? else {
            return Ok(crate::auto_hermetic::ProviderLockdownPolicy::Full);
        };
        let view = accounts.management.read().ok_or_else(|| {
            SessionHubError::Task("provider trust snapshot is unavailable".to_owned())
        })?;
        let summary = view
            .providers
            .iter()
            .find(|summary| summary.provider == provider);
        let has_active_credential = view
            .descriptors
            .iter()
            .any(|descriptor| descriptor.provider == provider && descriptor.active);
        Ok(crate::auto_hermetic::provider_policy(
            summary,
            has_active_credential,
        ))
    }

    /// Policy for the account adapter already selected for a turn. The
    /// resolver supplies `active_no_auth`; the management view supplies only
    /// the immutable provider shape and explicit trust floor.
    pub(crate) fn provider_lockdown_policy_for_active(
        &self,
        provider: &str,
        active_no_auth: bool,
    ) -> Result<crate::auto_hermetic::ProviderLockdownPolicy, SessionHubError> {
        if active_no_auth {
            let frozen = crate::auto_hermetic::provider_policy_for_active(None, true);
            if frozen.is_auto_hermetic() {
                return Ok(frozen);
            }
        }
        let Some(accounts) = self.accounts()? else {
            return Ok(crate::auto_hermetic::ProviderLockdownPolicy::Full);
        };
        let view = accounts.management.read().ok_or_else(|| {
            SessionHubError::Task("provider trust snapshot is unavailable".to_owned())
        })?;
        let summary = view
            .providers
            .iter()
            .find(|summary| summary.provider == provider);
        Ok(crate::auto_hermetic::provider_policy_for_active(
            summary,
            active_no_auth,
        ))
    }

    /// Atomically freezes one provider ceiling for a run from an authoritative
    /// turn input. The worker normally supplies the selected account fact;
    /// replayed child manifests can supply an already-frozen parent ceiling.
    /// A later exact auto-hermetic proposal may only narrow an older ordinary
    /// lockdown binding; no proposal can widen a bound run.
    pub(crate) fn bind_lockdown_turn(
        &self,
        session_id: &SessionId,
        run_id: &RunId,
        provider: &str,
        proposed_policy: crate::auto_hermetic::ProviderLockdownPolicy,
    ) -> Result<crate::auto_hermetic::ProviderLockdownPolicy, SessionHubError> {
        let (proposed_lockdown, proposed_auto_hermetic) = proposed_policy.binding_bits();
        let durable = match crate::lockdown::global() {
            Ok(manager) => {
                let profile_id = self.inner.store.cached_profile_installation_id();
                Some(
                    manager
                        .bind_turn(
                            profile_id,
                            session_id.as_str(),
                            run_id.as_str(),
                            provider,
                            proposed_lockdown,
                            proposed_auto_hermetic,
                        )
                        .map_err(|error| SessionHubError::Task(error.to_string()))?,
                )
            }
            #[cfg(test)]
            Err(_) => None,
            #[cfg(not(test))]
            Err(error) => return Err(SessionHubError::Task(error.to_string())),
        };
        let (provider, lockdown, auto_hermetic) = durable.unwrap_or_else(|| {
            (
                provider.to_owned(),
                proposed_lockdown,
                proposed_auto_hermetic,
            )
        });
        let policy =
            crate::auto_hermetic::ProviderLockdownPolicy::from_binding(lockdown, auto_hermetic);
        let mut turns = lock(&self.inner.lockdown_turns)?;
        let key = (session_id.clone(), run_id.clone());
        if let Some(binding) = turns.get_mut(&key) {
            if binding.provider != provider {
                return Err(SessionHubError::Task(format!(
                    "lockdown turn binding conflict for {session_id}/{run_id}: stored provider `{}`, requested `{provider}`",
                    binding.provider
                )));
            }
            if policy.is_auto_hermetic() && !binding.policy.is_auto_hermetic() {
                binding.policy = policy;
            }
            self.inner.lockdown_turn_bound.notify_waiters();
            return Ok(binding.policy);
        }
        turns.insert(key, LockdownTurnBinding { provider, policy });
        self.inner.lockdown_turn_bound.notify_waiters();
        Ok(policy)
    }

    /// Waits for the worker's resolved-account ceiling without allowing the
    /// hook engine to invent one from mutable account-management state. A
    /// bounded `None` result is fail-closed at the caller.
    pub(crate) async fn wait_bound_lockdown_run(
        &self,
        session_id: &SessionId,
        run_id: &RunId,
        timeout: Duration,
    ) -> Result<Option<(String, crate::auto_hermetic::ProviderLockdownPolicy)>, SessionHubError>
    {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.inner.lockdown_turn_bound.notified();
            if let Some(binding) = self.bound_lockdown_run(session_id, run_id)? {
                return Ok(Some(binding));
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.bound_lockdown_run(session_id, run_id);
            }
        }
    }

    /// Returns a frozen run's provider and ceiling. Headless runs may pin a
    /// provider different from the session metadata, so hook processing must
    /// consult this coordinate before it considers the metadata fallback.
    pub(crate) fn bound_lockdown_run(
        &self,
        session_id: &SessionId,
        run_id: &RunId,
    ) -> Result<Option<(String, crate::auto_hermetic::ProviderLockdownPolicy)>, SessionHubError>
    {
        let key = (session_id.clone(), run_id.clone());
        if let Some(binding) = lock(&self.inner.lockdown_turns)?.get(&key).cloned() {
            return Ok(Some((binding.provider, binding.policy)));
        }
        let binding = match crate::lockdown::global() {
            Ok(manager) => {
                let profile_id = self.inner.store.cached_profile_installation_id();
                manager
                    .turn_binding(profile_id, session_id.as_str(), run_id.as_str())
                    .map_err(|error| SessionHubError::Task(error.to_string()))?
            }
            #[cfg(test)]
            Err(_) => None,
            #[cfg(not(test))]
            Err(error) => return Err(SessionHubError::Task(error.to_string())),
        };
        if let Some((provider, lockdown, auto_hermetic)) = binding.as_ref() {
            lock(&self.inner.lockdown_turns)?.insert(
                key,
                LockdownTurnBinding {
                    provider: provider.clone(),
                    policy: crate::auto_hermetic::ProviderLockdownPolicy::from_binding(
                        *lockdown,
                        *auto_hermetic,
                    ),
                },
            );
        }
        Ok(binding.map(|(provider, lockdown, auto_hermetic)| {
            (
                provider,
                crate::auto_hermetic::ProviderLockdownPolicy::from_binding(lockdown, auto_hermetic),
            )
        }))
    }

    /// Marks the run whose boundary is now executing as the direct-control
    /// ceiling for this session. Durable per-run bindings remain available
    /// for crash recovery, while the live map sheds older completed/queued
    /// snapshots so a later Full boundary can restore Full direct controls.
    pub(crate) fn activate_lockdown_turn(
        &self,
        session_id: &SessionId,
        run_id: &RunId,
    ) -> Result<(), SessionHubError> {
        match crate::lockdown::global() {
            Ok(manager) => {
                let profile_id = self.inner.store.cached_profile_installation_id();
                manager
                    .activate_turn(profile_id, session_id.as_str(), run_id.as_str())
                    .map_err(|error| SessionHubError::Task(error.to_string()))?;
            }
            #[cfg(test)]
            Err(_) => {}
            #[cfg(not(test))]
            Err(error) => return Err(SessionHubError::Task(error.to_string())),
        }
        let key = (session_id.clone(), run_id.clone());
        let mut turns = lock(&self.inner.lockdown_turns)?;
        if !turns.contains_key(&key) {
            return Err(SessionHubError::Task(format!(
                "lockdown turn {session_id}/{run_id} was activated before it was bound"
            )));
        }
        turns.retain(|candidate, _| candidate.0 != *session_id || candidate == &key);
        Ok(())
    }

    /// Last provider ceiling that actually governed this session. Direct
    /// mutation surfaces use it until the next run boundary, including for a
    /// headless run whose provider differs from durable session metadata.
    pub(crate) fn bound_session_lockdown(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<(String, crate::auto_hermetic::ProviderLockdownPolicy)>, SessionHubError>
    {
        match crate::lockdown::global() {
            Ok(manager) => {
                let profile_id = self.inner.store.cached_profile_installation_id();
                Ok(manager
                    .active_session_binding(profile_id, session_id.as_str())
                    .map_err(|error| SessionHubError::Task(error.to_string()))?
                    .map(|(_, provider, lockdown, auto_hermetic)| {
                        (
                            provider,
                            crate::auto_hermetic::ProviderLockdownPolicy::from_binding(
                                lockdown,
                                auto_hermetic,
                            ),
                        )
                    }))
            }
            // Isolated hub tests may omit the process-global manager. In that
            // configuration `activate_lockdown_turn` prunes the local map to
            // its one active binding, so the fallback retains test parity.
            // Production must never inspect unactivated/queued bindings:
            // doing so could apply a trust toggle before the next boundary.
            #[cfg(test)]
            Err(_) => Ok(lock(&self.inner.lockdown_turns)?
                .iter()
                .find(|((candidate, _), _)| candidate == session_id)
                .map(|(_, binding)| (binding.provider.clone(), binding.policy))),
            #[cfg(not(test))]
            Err(error) => Err(SessionHubError::Task(error.to_string())),
        }
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

    /// Reads one background-task output payload from the profile CAS.
    pub(crate) async fn get_internal_artifact(
        &self,
        artifact: &haider_protocol::ids::ArtifactRef,
    ) -> Result<Vec<u8>, HaiderError> {
        self.inner.store.get(artifact).await
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
        drop(installed);
        // MONITOR intake starts only after the canonical turn engine exists.
        // Boot adoption can wake an already-expired durable watch; starting
        // earlier would allow acceptance to outrun the manager handoff.
        self.inner.monitors.activate(self.downgrade());
        Ok(())
    }

    pub(crate) fn install_peer_service(
        &self,
        service: Arc<crate::peer::PeerService>,
    ) -> Result<(), SessionHubError> {
        let mut installed = lock(&self.inner.peer_service)?;
        if installed.is_some() {
            return Err(SessionHubError::Task(
                "peer-messaging service is already installed".into(),
            ));
        }
        *installed = Some(service);
        Ok(())
    }

    pub(crate) fn peer_service(&self) -> Result<Arc<crate::peer::PeerService>, SessionHubError> {
        lock(&self.inner.peer_service)?
            .clone()
            .ok_or_else(|| SessionHubError::Task("peer-messaging service is not installed".into()))
    }

    pub(crate) fn install_loom_author_provider(
        &self,
        provider: Arc<dyn crate::worker::ProviderFactory>,
    ) -> Result<(), SessionHubError> {
        let mut installed = lock(&self.inner.loom_author_provider)?;
        if installed.is_some() {
            return Err(SessionHubError::Task(
                "Loom author provider is already installed".into(),
            ));
        }
        *installed = Some(provider);
        Ok(())
    }

    fn loom_author_provider(
        &self,
    ) -> Result<Arc<dyn crate::worker::ProviderFactory>, SessionHubError> {
        lock(&self.inner.loom_author_provider)?
            .clone()
            .ok_or_else(|| SessionHubError::Task("Loom author provider is not installed".into()))
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

    /// Opportunistically acknowledges completed durable hook work through the
    /// profile committer while a mutation is already admitted. A standalone
    /// acknowledgement retains its original direct transaction, so neither it
    /// nor a lone interactive turn pays a speculative batching delay.
    pub(crate) async fn complete_hook_dispatches(
        &self,
        acks: Vec<(SessionId, u64)>,
    ) -> Result<(), HaiderError> {
        if self.inner.append_committer.has_admitted_mutation() {
            self.inner
                .append_committer
                .complete_hook_dispatches(acks)
                .await
        } else {
            self.inner.store.complete_hook_dispatches(acks).await
        }
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
        if let Some(vault) = facade.vault.clone() {
            *lock(&self.inner.ssh)? = Some(crate::ssh::SshService::new(vault));
            // A pre-install compatibility read may only have observed the
            // synthetic legacy default. Once durable storage is available,
            // every scope must be resolved from that authority again.
            lock(&self.inner.ssh_scopes)?.clear();
            lock(&self.inner.ssh_scope_uncertain)?.clear();
        }
        *installed = Some(facade);
        Ok(())
    }

    pub(crate) fn ssh(&self) -> Result<Option<crate::ssh::SshService>, SessionHubError> {
        Ok(lock(&self.inner.ssh)?.clone())
    }

    pub(crate) fn ssh_scope(
        &self,
        session_id: &SessionId,
    ) -> Result<crate::ssh::SshScope, SessionHubError> {
        // Hold the cache lock through the durable read. This is deliberately
        // synchronous: otherwise a concurrent narrowing can be overwritten by
        // a cache-miss reader that loaded the older `All` value.
        let mut scopes = lock(&self.inner.ssh_scopes)?;
        if let Some(scope) = scopes.get(session_id).cloned() {
            return Ok(scope);
        }
        let Some(ssh) = lock(&self.inner.ssh)?.clone() else {
            // Do not cache this synthetic compatibility value: account/SSH
            // storage can be installed later in startup and may contain a
            // narrower durable scope.
            return Ok(crate::ssh::SshScope::All);
        };
        let scope = ssh
            .store
            .session_scope(session_id)
            .map_err(|error| SessionHubError::Task(error.to_string()))?;
        scopes.insert(session_id.clone(), scope.clone());
        Ok(scope)
    }

    pub(crate) fn set_ssh_scope(
        &self,
        session_id: SessionId,
        scope: crate::ssh::SshScope,
    ) -> Result<(), SessionHubError> {
        // Serialize persistence and cache publication with cache misses. A
        // caller can therefore never observe `All` after narrowing started.
        let mut scopes = lock(&self.inner.ssh_scopes)?;
        if let Some(ssh) = lock(&self.inner.ssh)?.as_ref() {
            if let Err(first_error) = ssh.store.set_session_scope(&session_id, &scope)
                && let Err(retry_error) = ssh.store.set_session_scope(&session_id, &scope)
            {
                lock(&self.inner.ssh_scope_uncertain)?.insert(session_id);
                return Err(SessionHubError::Task(format!(
                    "cannot commit SSH scope: {first_error}; retry failed: {retry_error}"
                )));
            }
        } else if !matches!(scope, crate::ssh::SshScope::All) {
            return Err(SessionHubError::Task(
                "SSH scope secret storage is unavailable".into(),
            ));
        }
        lock(&self.inner.ssh_scope_uncertain)?.remove(&session_id);
        scopes.insert(session_id, scope);
        Ok(())
    }

    /// Publishes the implicit `All` scope only after a new session creation
    /// commits. This must never run for a receipt/idempotent replay: after a
    /// restart the empty cache may hide a narrower durable scope. `or_insert`
    /// also preserves a narrowing that races the fresh commit's publication.
    pub(crate) fn cache_default_ssh_scope_after_create(
        &self,
        session_id: SessionId,
    ) -> Result<(), SessionHubError> {
        lock(&self.inner.ssh_scopes)?
            .entry(session_id)
            .or_insert(crate::ssh::SshScope::All);
        Ok(())
    }

    /// Snapshots the source scope and durably installs the exact same value
    /// for a candidate fork while holding the scope serialization lock. A
    /// missing source record legitimately decodes as historical `All`, but
    /// the child always receives an explicit record, including for `None`.
    fn clone_ssh_scope_for_fork(
        &self,
        source_session_id: &SessionId,
        child_session_id: &SessionId,
    ) -> Result<(), SessionHubError> {
        let mut scopes = lock(&self.inner.ssh_scopes)?;
        let ssh = lock(&self.inner.ssh)?.clone().ok_or_else(|| {
            SessionHubError::Task("SSH scope secret storage is unavailable".into())
        })?;
        if lock(&self.inner.ssh_scope_uncertain)?.contains(source_session_id) {
            return Err(SessionHubError::Task(
                "source SSH scope has an unresolved durable write".into(),
            ));
        }
        if scopes.contains_key(child_session_id)
            || ssh
                .store
                .session_scope_if_present(child_session_id)
                .map_err(|error| SessionHubError::Task(error.to_string()))?
                .is_some()
        {
            return Err(SessionHubError::Task(
                "fork child already has an SSH scope".into(),
            ));
        }
        let source_scope = if let Some(scope) = scopes.get(source_session_id).cloned() {
            scope
        } else {
            let scope = ssh
                .store
                .session_scope(source_session_id)
                .map_err(|error| SessionHubError::Task(error.to_string()))?;
            scopes.insert(source_session_id.clone(), scope.clone());
            scope
        };
        if let Err(first_error) = ssh.store.set_session_scope(child_session_id, &source_scope) {
            // FileVault can rename the exact bytes and then fail its directory
            // fsync. A read-back would prove only current visibility, not
            // crash durability, so require one complete retry to report a
            // successful durable commit before SQLite may create the child.
            if let Err(retry_error) = ssh.store.set_session_scope(child_session_id, &source_scope) {
                let _ = ssh.store.delete_session_scope(child_session_id);
                return Err(SessionHubError::Task(format!(
                    "cannot commit fork SSH scope: {first_error}; retry failed: {retry_error}"
                )));
            }
        }
        scopes.insert(child_session_id.clone(), source_scope);
        Ok(())
    }

    fn cache_committed_fork_scope(
        &self,
        child_session_id: &SessionId,
    ) -> Result<(), SessionHubError> {
        let mut scopes = lock(&self.inner.ssh_scopes)?;
        if scopes.contains_key(child_session_id) {
            return Ok(());
        }
        let ssh = lock(&self.inner.ssh)?.clone().ok_or_else(|| {
            SessionHubError::Task("SSH scope secret storage is unavailable".into())
        })?;
        let scope = ssh
            .store
            .session_scope_if_present(child_session_id)
            .map_err(|error| SessionHubError::Task(error.to_string()))?
            .ok_or_else(|| {
                SessionHubError::Task("committed fork is missing its explicit SSH scope".into())
            })?;
        scopes.insert(child_session_id.clone(), scope);
        Ok(())
    }

    fn discard_provisional_ssh_scope(
        &self,
        child_session_id: &SessionId,
    ) -> Result<(), SessionHubError> {
        let mut scopes = lock(&self.inner.ssh_scopes)?;
        let ssh = lock(&self.inner.ssh)?.clone().ok_or_else(|| {
            SessionHubError::Task("SSH scope secret storage is unavailable".into())
        })?;
        let deleted = ssh
            .store
            .delete_session_scope(child_session_id)
            .map_err(|error| SessionHubError::Task(error.to_string()));
        scopes.remove(child_session_id);
        deleted
    }

    async fn discard_fork_scope_if_session_absent(&self, child_session_id: &SessionId) {
        match self.inner.store.session_ids().await {
            Ok(session_ids) if !session_ids.contains(child_session_id) => {
                if let Err(error) = self.discard_provisional_ssh_scope(child_session_id) {
                    tracing::warn!(
                        session_id = %child_session_id,
                        %error,
                        "could not remove an uncommitted fork SSH scope"
                    );
                }
            }
            Ok(_) => {
                tracing::warn!(
                    session_id = %child_session_id,
                    "retaining fork SSH scope because a durable child exists"
                );
            }
            Err(error) => {
                tracing::warn!(
                    session_id = %child_session_id,
                    %error,
                    "retaining fork SSH scope because child absence could not be proved"
                );
            }
        }
    }

    fn reserve_fork_candidate(
        &self,
        session_id: &SessionId,
    ) -> Result<ForkCandidateReservation, SessionHubError> {
        {
            let mut candidates = lock(&self.inner.fork_candidates)?;
            if !candidates.insert(session_id.clone()) {
                return Err(SessionHubError::Task(
                    "fork child session id is already reserved".into(),
                ));
            }
        }
        let reservation = ForkCandidateReservation {
            inner: Arc::clone(&self.inner),
            session_id: session_id.clone(),
            committed: false,
        };
        if lock(&self.inner.actors)?.contains_key(session_id) {
            return Err(SessionHubError::Task(
                "fork child session id already has a live actor".into(),
            ));
        }
        Ok(reservation)
    }

    pub(crate) fn shell_registry(&self) -> &crate::shell_registry::ShellRegistry {
        &self.inner.shells
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

    pub(crate) async fn capture_haider_code_plan_status(
        &self,
        account_alias: CredentialAlias,
        snapshot: haider_protocol::usage::HaiderCodePlanSnapshotV1,
        meter: crate::haider_code_plan::PlanMeterValues,
    ) -> Result<(), SessionHubError> {
        if let Some(service) = self.usage_report_service()? {
            service
                .capture_haider_code_plan_status(&self.inner.store, account_alias, snapshot, meter)
                .await?;
        }
        Ok(())
    }

    pub(crate) async fn clear_haider_code_plan_status(
        &self,
        account_alias: &CredentialAlias,
    ) -> Result<(), SessionHubError> {
        if let Some(service) = self.usage_report_service()? {
            service.clear_haider_code_plan_status(account_alias).await;
        }
        Ok(())
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

    pub(crate) async fn peer_session_summaries(
        &self,
    ) -> Result<Vec<SessionSummary>, SessionHubError> {
        // Actors are the daemon's resident agents. Unlike the store-wide
        // session index this excludes closed historical sessions while still
        // retaining a detached agent that can receive background peer work.
        let ids = lock(&self.inner.actors)?
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        rpc::session_summaries(self, &ids).await
    }

    pub(crate) fn subscribe_peer_reconcile(&self) -> broadcast::Receiver<SessionId> {
        self.inner.roster_publications.subscribe()
    }

    pub(crate) fn peer_control_sessions(
        &self,
        connection_id: &str,
    ) -> Result<Vec<SessionId>, SessionHubError> {
        let mut sessions = lock(&self.inner.attachments)?
            .values()
            .filter(|owner| {
                owner.connection_id == connection_id && owner.mode == AttachMode::Control
            })
            .map(|owner| owner.session_id.clone())
            .collect::<Vec<_>>();
        sessions.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        sessions.dedup();
        Ok(sessions)
    }

    pub(crate) fn enable_peer_events(&self, connection_id: &str) -> Result<(), SessionHubError> {
        lock(&self.inner.peer_event_subscribers)?.insert(connection_id.to_owned());
        Ok(())
    }

    pub(crate) fn publish_peer_event(&self, session_id: &SessionId, frame: WireFrame) {
        let Ok(subscribers) = self.inner.peer_event_subscribers.lock() else {
            return;
        };
        let Ok(attachments) = self.inner.attachments.lock() else {
            return;
        };
        let recipients = attachments
            .values()
            .filter(|owner| owner.session_id == *session_id)
            .map(|owner| owner.connection_id.as_str())
            .filter(|connection_id| peer_event_allowed(&subscribers, connection_id))
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

    /// Acquires the ordinary admission serial and returns a claim token only
    /// while the target is idle. The caller durably appends its shared-mailbox
    /// claim before passing the token to `accept_claimed_peer_turn`.
    pub(crate) async fn begin_peer_turn_claim(
        &self,
        message: &haider_protocol::peer::PeerMessage,
    ) -> Result<Option<PeerTurnClaim>, SessionHubError> {
        if self.inner.draining.load(Ordering::Acquire) {
            return Err(SessionHubError::Closed);
        }
        let session_id = SessionId::new(message.to.clone());
        let selection = self.lock_workflow_selection(&session_id).await;
        if lock(&self.inner.deleting_sessions)?.contains(&session_id) {
            return Err(SessionHubError::Store(HaiderError::new(
                ErrorCode::InvalidArgument,
                "session was deleted",
                false,
            )));
        }
        if self.session_has_nonterminal_runs(&session_id).await? {
            return Ok(None);
        }
        Ok(Some(PeerTurnClaim {
            session_id,
            _selection: selection,
        }))
    }

    /// Commits a peer turn while retaining the idle fence acquired before the
    /// caller's mailbox claim. There is therefore no unclaimed core commit.
    pub(crate) async fn accept_claimed_peer_turn(
        &self,
        message: &haider_protocol::peer::PeerMessage,
        claim: PeerTurnClaim,
    ) -> Result<(AcceptedTurn, bool), SessionHubError> {
        if claim.session_id.as_str() != message.to.as_str() {
            return Err(SessionHubError::Task(
                "peer turn claim target does not match its message".into(),
            ));
        }
        let (command_id, request_digest, request_json) = peer_turn_coordinates(message)?;
        let command = TurnAcceptCommand {
            command_id,
            request_digest,
            request_json,
            session_id: claim.session_id.clone(),
            worker_generation: self.inner.store.worker_generation(),
            run_id: RunId::new(random_id("peer-run")?),
            agent_id: None,
            branch_id: None,
            text: message.render_for_prompt(),
            attachments: Vec::new(),
            mode: haider_protocol::DeliveryMode::Queue,
            queued_event_id: EventId::new(random_id("peer-queued")?),
            user_event_id: EventId::new(random_id("peer-message")?),
            active_event_id: EventId::new(random_id("peer-active")?),
            device_id: self.inner.device_id.clone(),
        };
        let actor = self.actor_for(claim.session_id.clone()).await?;
        let (completed, result) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::AcceptTurn {
                command,
                peer_message: Some(message.clone()),
                auto_title: None,
                completed,
            })
            .await
            .map_err(|_| SessionHubError::Closed)?;
        let outcome = result
            .await
            .map_err(|_| SessionHubError::Closed)?
            .map_err(SessionHubError::from)?;
        let (accepted, fresh) = match outcome {
            TurnAcceptOutcome::Committed { accepted, .. } => (accepted, true),
            TurnAcceptOutcome::IdempotentReplay { accepted } => (accepted, false),
        };
        if accepted.disposition != TurnAdmissionDisposition::Started {
            return Err(SessionHubError::Task(
                "idle-fenced peer delivery was not admitted as a fresh turn".into(),
            ));
        }
        Ok((accepted, fresh))
    }

    /// Reads the durable core admission receipt used to reconcile a crash
    /// after turn acceptance but before the peer mailbox records `Accepted`.
    pub(crate) async fn peer_turn_receipt(
        &self,
        message: &haider_protocol::peer::PeerMessage,
    ) -> Result<Option<AcceptedTurn>, SessionHubError> {
        let (command_id, request_digest, request_json) = peer_turn_coordinates(message)?;
        self.inner
            .store
            .turn_accept_receipt(command_id, request_digest, request_json)
            .await
            .map_err(Into::into)
    }

    /// Recreates the resident actor that production startup turn recovery
    /// installs before peer-mailbox reconciliation.
    #[cfg(test)]
    pub(crate) async fn ensure_peer_session_actor_for_test(
        &self,
        session_id: SessionId,
    ) -> Result<(), SessionHubError> {
        let _actor = self.actor_for(session_id).await?;
        Ok(())
    }

    pub(crate) async fn handoff_peer_turn(
        &self,
        accepted: AcceptedTurn,
    ) -> Result<(), SessionHubError> {
        self.worker_manager()?
            .submit(accepted)
            .await
            .map_err(SessionHubError::from)?;
        #[cfg(test)]
        self.inner
            .peer_handoff_count
            .fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn peer_handoff_count_for_test(&self) -> u64 {
        self.inner.peer_handoff_count.load(Ordering::Relaxed)
    }

    /// C1 — one registered Loom workflow (worker's typed-node tail).
    pub(crate) async fn loom_workflow(
        &self,
        id: &str,
    ) -> Result<Option<haider_protocol::loom::LoomWorkflow>, HaiderError> {
        self.inner.store.loom_workflow(id.to_owned()).await
    }

    /// Resolves the immutable registry revision named by a pinned graph fact.
    pub(crate) async fn loom_workflow_revision(
        &self,
        id: &str,
        template_digest: &str,
    ) -> Result<Option<haider_protocol::loom::LoomWorkflow>, HaiderError> {
        self.inner
            .store
            .loom_workflow_revision(id.to_owned(), template_digest.to_owned())
            .await
    }

    /// Runtime-only exact lookup. A current registry row without the pinned
    /// historical revision is corruption, never permission to execute the
    /// graph as an untyped built-in/one-off template.
    pub(crate) async fn pinned_loom_workflow(
        &self,
        id: &str,
        template_digest: &str,
    ) -> Result<Option<haider_protocol::loom::LoomWorkflow>, HaiderError> {
        if let Some(workflow) = self.loom_workflow_revision(id, template_digest).await? {
            return Ok(Some(workflow));
        }
        if self.loom_workflow(id).await?.is_some() {
            return Err(HaiderError::new(
                ErrorCode::StoreCorrupt,
                format!(
                    "pinned Loom workflow `{id}` revision `{template_digest}` is missing from history"
                ),
                false,
            ));
        }
        Ok(None)
    }

    /// C2 — one registered Loom agent type (typed spawns).
    pub(crate) async fn loom_agent_type(
        &self,
        id: &str,
    ) -> Result<Option<haider_protocol::loom::LoomAgentType>, HaiderError> {
        self.inner.store.loom_agent_type(id.to_owned()).await
    }

    /// Exact retained lookup for a workflow node's frozen agent contract.
    pub(crate) async fn loom_agent_type_revision(
        &self,
        id: &str,
        rev: u32,
        digest: &str,
    ) -> Result<Option<haider_protocol::loom::LoomAgentType>, HaiderError> {
        self.inner
            .store
            .loom_agent_type_revision(id.to_owned(), rev, digest.to_owned())
            .await
    }

    /// Resolve exactly the agent contract frozen into workflow metadata.
    /// Legacy/unbound metadata deliberately falls through to the current row
    /// so the typed dispatcher can return its established contract-unbound
    /// refusal instead of silently treating the node as a control node.
    pub(crate) async fn pinned_loom_agent_type(
        &self,
        id: &str,
        rev: Option<u32>,
        digest: Option<&str>,
    ) -> Result<Option<haider_protocol::loom::LoomAgentType>, HaiderError> {
        let (Some(rev), Some(digest)) = (rev, digest) else {
            return self.loom_agent_type(id).await;
        };
        if let Some(record) = self.loom_agent_type_revision(id, rev, digest).await? {
            return Ok(Some(record));
        }
        let current = self.loom_agent_type(id).await?;
        if let Some(record) = current.as_ref()
            && record.rev == rev
            && record.digest() == digest
        {
            // Upgrade compatibility: migration backfills the current row into
            // retained history, but an older store implementation can still
            // race this read while the current contract remains unchanged.
            return Ok(current);
        }
        // A registration can publish history between the first miss and the
        // current-row read. Retry once before diagnosing a missing revision.
        if let Some(record) = self.loom_agent_type_revision(id, rev, digest).await? {
            return Ok(Some(record));
        }
        if current.is_some() {
            return Err(HaiderError::new(
                ErrorCode::StoreCorrupt,
                format!(
                    "pinned Loom agent type `{id}` revision {rev} `{digest}` is missing from history"
                ),
                false,
            ));
        }
        Ok(None)
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

    pub(crate) async fn loom_register_workflow_cas(
        &self,
        source: String,
        expected: haider_protocol::loom::LoomRevisionExpectation,
    ) -> Result<
        haider_core::LoomRegistryMutation<haider_protocol::loom::LoomRegistration>,
        HaiderError,
    > {
        let outcome = self
            .inner
            .store
            .loom_register_workflow_cas(source, expected)
            .await?;
        if let haider_core::LoomRegistryMutation::Applied {
            publication_cursor: Some(cursor),
            ..
        } = &outcome
        {
            let _ = self.inner.loom_registry_publications.send(*cursor);
        }
        Ok(outcome)
    }

    pub(crate) async fn loom_register_agent_type_cas(
        &self,
        record: haider_protocol::loom::LoomAgentType,
        expected: haider_protocol::loom::LoomRevisionExpectation,
    ) -> Result<
        haider_core::LoomRegistryMutation<haider_core::LoomAgentTypeRegistration>,
        HaiderError,
    > {
        if self.inner.draining.load(Ordering::Acquire) {
            return Err(HaiderError::new(
                ErrorCode::Busy,
                "daemon is draining; typed-agent registration is closed",
                true,
            ));
        }
        // Registry revision changes and installer adoption share one daemon
        // ownership lane. This prevents startup from deciding an old
        // revision is current while a concurrent registration supersedes it.
        let install_serial = self.inner.typed_install_serial.lock().await;
        if self.inner.draining.load(Ordering::Acquire) {
            return Err(HaiderError::new(
                ErrorCode::Busy,
                "daemon is draining; typed-agent registration is closed",
                true,
            ));
        }
        let outcome = self
            .inner
            .store
            .loom_register_agent_type_with_install_cas(record, expected)
            .await?;
        drop(install_serial);
        if let haider_core::LoomRegistryMutation::Applied {
            value,
            publication_cursor,
        } = &outcome
        {
            if let Some(cursor) = publication_cursor {
                let _ = self.inner.loom_registry_publications.send(*cursor);
            }
            if let Some(job) = &value.install_job {
                self.spawn_typed_install_job(job.job_id.clone())?;
            }
        }
        Ok(outcome)
    }

    pub(crate) async fn typed_agent_install_jobs(
        &self,
        job_id: Option<String>,
        agent_type_id: Option<String>,
    ) -> Result<Vec<haider_protocol::typed_agent::TypedAgentInstallJob>, HaiderError> {
        self.inner
            .store
            .typed_agent_install_jobs(job_id, agent_type_id)
            .await
    }

    pub(crate) async fn typed_agent_install_status(
        &self,
        job_id: Option<String>,
        agent_type_id: Option<String>,
    ) -> Result<haider_core::TypedAgentInstallSnapshot, HaiderError> {
        self.inner
            .store
            .typed_agent_install_status(job_id, agent_type_id)
            .await
    }

    pub(crate) async fn typed_agent_install_retry(
        &self,
        job_id: String,
    ) -> Result<haider_core::TypedAgentInstallRetryResult, HaiderError> {
        if self.inner.draining.load(Ordering::Acquire) {
            return Err(HaiderError::new(
                ErrorCode::Busy,
                "daemon is draining; typed-agent install retry is closed",
                true,
            ));
        }
        let install_serial = self.inner.typed_install_serial.lock().await;
        if self.inner.draining.load(Ordering::Acquire) {
            return Err(HaiderError::new(
                ErrorCode::Busy,
                "daemon is draining; typed-agent install retry is closed",
                true,
            ));
        }
        let outcome = self.inner.store.typed_agent_install_retry(job_id).await?;
        let requeued_job_id = match &outcome {
            haider_core::TypedAgentInstallRetryResult::Requeued(job) => Some(job.job_id.clone()),
            _ => None,
        };
        drop(install_serial);
        if let Some(job_id) = requeued_job_id {
            self.spawn_typed_install_job(job_id)?;
        }
        Ok(outcome)
    }

    pub(crate) async fn typed_agent_install_watch(
        &self,
        job_id: String,
        after_cursor: u64,
    ) -> Result<haider_core::TypedAgentInstallWatchResult, HaiderError> {
        self.inner
            .store
            .typed_agent_install_watch(job_id, after_cursor)
            .await
    }

    pub(crate) async fn typed_agent_install_cancel(
        &self,
        install_job_id: String,
    ) -> Result<haider_core::TypedAgentInstallCancelResult, HaiderError> {
        if self.inner.draining.load(Ordering::Acquire) {
            return Err(HaiderError::new(
                ErrorCode::Busy,
                "daemon is draining; typed-agent install cancellation is closed",
                true,
            ));
        }
        // Deliberately do not take `typed_install_serial`: cancellation races
        // the runner's next durable CAS and makes that runner lose. Holding the
        // process-wide package-manager lane would make a running job
        // uncancellable until it had already become terminal.
        self.inner
            .store
            .typed_agent_install_cancel(install_job_id)
            .await
    }

    pub(crate) async fn loom_set_archived(
        &self,
        kind: haider_protocol::loom::LoomRegistryEntryKind,
        id: String,
        archived: bool,
        expected: haider_protocol::loom::LoomRevisionExpectation,
    ) -> Result<haider_core::LoomArchiveResult, HaiderError> {
        let outcome = self
            .inner
            .store
            .loom_set_archived(kind, id, archived, expected)
            .await?;
        if let haider_core::LoomArchiveResult::Changed {
            publication_cursor, ..
        } = &outcome
        {
            let _ = self
                .inner
                .loom_registry_publications
                .send(*publication_cursor);
        }
        Ok(outcome)
    }

    /// Narrow daemon-internal session creation used by local delegation.
    /// It preserves the same unfenced receipt preflight and actor-routed
    /// transaction as the wire method without fabricating an RPC connection.
    #[cfg(test)]
    pub(crate) async fn create_internal_session(
        &self,
        command: SessionCreateCommand,
    ) -> Result<CreatedSession, HaiderError> {
        self.create_internal_session_with_interaction_mode(
            command,
            haider_protocol::session::SessionInteractionModeV1::Interactive,
        )
        .await
    }

    pub(crate) async fn create_internal_session_with_interaction_mode(
        &self,
        command: SessionCreateCommand,
        interaction_mode: haider_protocol::session::SessionInteractionModeV1,
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
            .create_session_with_interaction_mode(command, interaction_mode, None)
            .await
            .map_err(hub_error_as_store)?
        {
            SessionCreateOutcome::Committed { created, .. }
            | SessionCreateOutcome::IdempotentReplay { created } => Ok(created),
        }
    }

    /// Narrow daemon-internal turn acceptance used by local delegation. The
    /// store transaction performs receipt replay before its current-state
    /// fences, so a separate replay read would only add a blocking-pool and
    /// SQLite boundary.
    pub(crate) async fn accept_internal_turn(
        &self,
        command: TurnAcceptCommand,
    ) -> Result<AcceptedTurn, HaiderError> {
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

    /// Enqueue a persist-before-wake cancellation without waiting for its
    /// receipt. Delegation uses this only after the run's absolute wait budget
    /// is already exhausted: the pending mirror fact is durable first, this
    /// nonblocking wake prevents a live orphan, and startup recovery remains
    /// the repair owner if the daemon dies before the actor consumes it.
    pub(crate) fn try_cancel_internal_turn(
        &self,
        command: TurnCancelCommand,
    ) -> Result<(), HaiderError> {
        let actor = self
            .existing_actor(&command.session_id)
            .map_err(hub_error_as_store)?
            .ok_or_else(|| {
                HaiderError::new(
                    ErrorCode::RunNotActive,
                    "live child session has no actor for cancellation wake",
                    false,
                )
            })?;
        let (completed, _response) = oneshot::channel();
        match actor
            .commands
            .try_send(ActorCommand::CancelTurn { command, completed })
        {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(command)) => {
                // The parent may not wait beyond its exhausted deadline. Keep
                // an in-process owner for the already-durable cancellation
                // wake; a concurrent daemon crash is repaired from the
                // journaled mirror-pending fact at startup.
                tokio::spawn(async move {
                    if actor.commands.send(command).await.is_err() {
                        tracing::warn!(
                            "child session actor closed before queued cancellation wake"
                        );
                    }
                });
                Ok(())
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(hub_closed_store_error()),
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
        let outcome = self.inner.store.create_delegation(record).await?;
        self.notify_descendant_lineage_change();
        Ok(outcome)
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
        let record = self.inner.store.mark_delegation_running(agent).await?;
        self.notify_descendant_lineage_change();
        Ok(record)
    }

    pub(crate) async fn record_delegation_report(
        &self,
        agent: haider_protocol::ids::AgentId,
        report: haider_protocol::agent::ChildReport,
    ) -> Result<haider_core::DelegationRecord, HaiderError> {
        let record = self
            .inner
            .store
            .record_delegation_report(agent, report)
            .await?;
        self.notify_descendant_lineage_change();
        Ok(record)
    }

    pub(crate) async fn mark_delegation_collected(
        &self,
        agent: haider_protocol::ids::AgentId,
    ) -> Result<haider_core::DelegationRecord, HaiderError> {
        let record = self.inner.store.mark_delegation_collected(agent).await?;
        self.notify_descendant_lineage_change();
        Ok(record)
    }

    fn notify_descendant_lineage_change(&self) {
        self.inner
            .descendant_lineage_publications
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }

    pub(crate) async fn read_internal_session(
        &self,
        session_id: &SessionId,
        since_seq: u64,
        limit: usize,
    ) -> Result<Vec<RawEnvelope>, HaiderError> {
        self.inner.store.read(session_id, since_seq, limit).await
    }

    pub(crate) async fn latest_internal_session_seq(
        &self,
        session_id: &SessionId,
    ) -> Result<u64, HaiderError> {
        self.inner.store.latest_seq(session_id).await
    }

    pub(crate) async fn monitor_control_receipt(
        &self,
        command_id: &haider_rpc::CommandId,
        method: &str,
        request_digest: &str,
        request_json: &str,
    ) -> Result<Option<serde_json::Value>, HaiderError> {
        self.inner
            .store
            .monitor_control_receipt(
                command_id.as_str().to_owned(),
                method.to_owned(),
                request_digest.to_owned(),
                request_json.to_owned(),
            )
            .await
    }

    pub(crate) async fn claim_monitor_control_receipt(
        &self,
        command_id: &haider_rpc::CommandId,
        method: &str,
        request_digest: &str,
        request_json: &str,
    ) -> Result<haider_core::MonitorControlClaim, HaiderError> {
        self.inner
            .store
            .claim_monitor_control_receipt(
                command_id.as_str().to_owned(),
                method.to_owned(),
                request_digest.to_owned(),
                request_json.to_owned(),
            )
            .await
    }

    pub(crate) async fn finalize_monitor_control_receipt(
        &self,
        command_id: &haider_rpc::CommandId,
        session_id: &SessionId,
        accepted_seq: u64,
        response: serde_json::Value,
    ) -> Result<(), HaiderError> {
        self.inner
            .store
            .finalize_monitor_control_receipt(
                command_id.as_str().to_owned(),
                session_id.clone(),
                accepted_seq,
                response,
            )
            .await
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
        self.open_connection_with_runtime_paths(capabilities, sink, transport, None, None, false)
    }

    pub(crate) fn open_connection_with_runtime_paths(
        &self,
        capabilities: CapabilitySet,
        sink: Arc<dyn FrameSink>,
        transport: crate::accounts::ConnectionTransport,
        runtime_paths: Option<(std::path::PathBuf, std::path::PathBuf)>,
        daemon_idle_ttl_ms: Option<u64>,
        daemon_warm: bool,
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
        let (loom_author_cancel, _) = watch::channel(false);
        let identity_lease = Arc::new(ConnectionIdentityLease {
            hub: self.clone(),
            connection_id: connection_id.clone(),
            loom_author_cancel,
        });
        Ok(HubConnection {
            hub: self.clone(),
            connection_id,
            capabilities,
            sink,
            transport,
            runtime_paths,
            daemon_idle_ttl_ms,
            daemon_warm,
            stages: Mutex::new(crate::accounts::StagedSecrets::default()),
            roster_watch: Mutex::new(None),
            accounts_watch: Mutex::new(None),
            surface_watch: Mutex::new(None),
            monitor_watch: Mutex::new(None),
            loom_registry_watch: Mutex::new(None),
            loom_registry_watch_serial: tokio::sync::Mutex::new(()),
            metafork_reviews: Arc::new(Mutex::new(HashMap::new())),
            loom_author_sessions: Arc::new(Mutex::new(HashMap::new())),
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
    #[cfg(test)]
    async fn create_session(
        &self,
        command: SessionCreateCommand,
    ) -> Result<SessionCreateOutcome, SessionHubError> {
        self.create_session_with_interaction_mode(
            command,
            haider_protocol::session::SessionInteractionModeV1::Interactive,
            None,
        )
        .await
    }

    async fn create_session_with_interaction_mode(
        &self,
        command: SessionCreateCommand,
        interaction_mode: haider_protocol::session::SessionInteractionModeV1,
        account_alias: Option<String>,
    ) -> Result<SessionCreateOutcome, SessionHubError> {
        let actor = self.actor_for(command.session_id.clone()).await?;
        let (completed, result) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::CreateSession {
                command,
                interaction_mode,
                account_alias,
                completed,
            })
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

    /// Owns the complete fork attempt in a hub task. A disconnected RPC may
    /// stop awaiting the result, but it cannot cancel the scope/store
    /// reconciliation halfway through and accidentally expose a child whose
    /// explicit scope was removed.
    async fn fork_session(
        &self,
        command: SessionForkCommand,
    ) -> Result<SessionForkOutcome, SessionHubError> {
        self.fork_session_request(SessionForkRequest::Exact(command))
            .await
    }

    async fn fork_session_from_prompt(
        &self,
        command: SessionPromptForkCommand,
    ) -> Result<SessionForkOutcome, SessionHubError> {
        self.fork_session_request(SessionForkRequest::Prompt(command))
            .await
    }

    async fn fork_session_request(
        &self,
        command: SessionForkRequest,
    ) -> Result<SessionForkOutcome, SessionHubError> {
        let hub = self.clone();
        let (completed, result) = oneshot::channel();
        let task = tokio::spawn(async move {
            let outcome = hub.fork_session_owned(command).await;
            let _ = completed.send(outcome);
        });
        lock(&self.inner.actor_tasks)?.push(task);
        result.await.map_err(|_| SessionHubError::Closed)?
    }

    async fn fork_session_owned(
        &self,
        command: SessionForkRequest,
    ) -> Result<SessionForkOutcome, SessionHubError> {
        let _publication_serial = self.inner.fork_publication_serial.lock().await;
        let (
            receipt_command_id,
            request_digest,
            request_json,
            metafork,
            candidate_session_id,
            source_session_id,
        ) = match &command {
            SessionForkRequest::Exact(command) => (
                CommandId::new(command.command_id.clone()),
                command.request_digest.clone(),
                command.request_json.clone(),
                command.metafork.is_some(),
                command.session_id.clone(),
                command.source_session_id.clone(),
            ),
            SessionForkRequest::Prompt(command) => (
                CommandId::new(command.command_id.clone()),
                command.request_digest.clone(),
                command.request_json.clone(),
                false,
                command.session_id.clone(),
                command.source_session_id.clone(),
            ),
        };
        if let Some(created) = self
            .session_fork_receipt(
                &receipt_command_id,
                &request_digest,
                &request_json,
                metafork,
            )
            .await?
        {
            self.cache_committed_fork_scope(&created.session_id)?;
            self.publish_committed_fork(&created, None).await?;
            return Ok(SessionForkOutcome::IdempotentReplay { created });
        }
        let mut reservation = Some(self.reserve_fork_candidate(&candidate_session_id)?);
        if self
            .inner
            .store
            .session_ids()
            .await?
            .contains(&candidate_session_id)
        {
            return Err(SessionHubError::Store(HaiderError::new(
                ErrorCode::InvalidArgument,
                "daemon-minted child session id already exists",
                false,
            )));
        }
        self.clone_ssh_scope_for_fork(&source_session_id, &candidate_session_id)?;
        let stored = match command {
            SessionForkRequest::Exact(command) => self.inner.store.fork_session(command).await,
            SessionForkRequest::Prompt(command) => {
                self.inner.store.fork_session_from_prompt(command).await
            }
        };
        let outcome = match stored {
            Ok(outcome) => outcome,
            Err(error) => {
                // SQLite COMMIT errors can be ambiguous. Reconcile the durable
                // receipt before any compensation; deleting scope while the
                // child actually committed would reopen absent-means-`All`.
                match self
                    .session_fork_receipt(
                        &receipt_command_id,
                        &request_digest,
                        &request_json,
                        metafork,
                    )
                    .await
                {
                    Ok(Some(created)) => {
                        if created.session_id == candidate_session_id
                            && let Some(reservation) = reservation.as_mut()
                        {
                            reservation.retain_until_published();
                        }
                        if created.session_id != candidate_session_id {
                            self.discard_fork_scope_if_session_absent(&candidate_session_id)
                                .await;
                        }
                        self.cache_committed_fork_scope(&created.session_id)?;
                        self.publish_committed_fork(&created, None).await?;
                        return Ok(SessionForkOutcome::IdempotentReplay { created });
                    }
                    Ok(None) => {
                        self.discard_fork_scope_if_session_absent(&candidate_session_id)
                            .await;
                    }
                    Err(reconcile_error) => {
                        tracing::warn!(
                            session_id = %candidate_session_id,
                            error = %reconcile_error,
                            "retaining fork SSH scope after ambiguous commit failure"
                        );
                    }
                }
                return Err(error.into());
            }
        };
        match &outcome {
            SessionForkOutcome::Committed { created, envelopes } => {
                if let Some(reservation) = reservation.as_mut() {
                    reservation.retain_until_published();
                }
                if let Some(last) = envelopes.last() {
                    self.inner.observer.observe(HubObservation::Persisted {
                        session_id: candidate_session_id.clone(),
                        through_seq: last.seq,
                    });
                }
                self.publish_committed_fork(created, Some(envelopes))
                    .await?;
            }
            SessionForkOutcome::IdempotentReplay { created } => {
                // A racing command may have won after the initial receipt
                // preflight. Its child scope is authority; the losing
                // candidate remains unobservable and can be discarded.
                if created.session_id != candidate_session_id {
                    self.discard_fork_scope_if_session_absent(&candidate_session_id)
                        .await;
                    self.cache_committed_fork_scope(&created.session_id)?;
                } else if let Some(reservation) = reservation.as_mut() {
                    reservation.retain_until_published();
                }
                self.publish_committed_fork(created, None).await?;
            }
        }
        Ok(outcome)
    }

    /// Completes the one publication barrier for a durable fork. The child
    /// stays in `fork_candidates` until its ordinary actor exists and the
    /// native Pipe proves coverage through the committed child head. Removing
    /// that fence is the visibility linearization point; the normal commit
    /// projection then wakes roster observers.
    async fn publish_committed_fork(
        &self,
        created: &CreatedSessionFork,
        committed: Option<&[RawEnvelope]>,
    ) -> Result<(), SessionHubError> {
        let actor_ready = self.existing_actor(&created.session_id)?.is_some();
        let pipe_ready = self
            .inner
            .pipe_native
            .confirms_coverage(&created.session_id, created.created_seq);
        {
            let mut candidates = lock(&self.inner.fork_candidates)?;
            if !candidates.contains(&created.session_id) && actor_ready && pipe_ready {
                return Ok(());
            }
            // Receipt replay may be the first process-local knowledge of a
            // fork whose durable transaction committed before a crash.
            // Reinstall the visibility fence before any repair await.
            candidates.insert(created.session_id.clone());
        }
        self.cache_committed_fork_scope(&created.session_id)?;
        self.actor_for_inner(created.session_id.clone(), true)
            .await?;
        self.inner
            .pipe_native
            .maintain_and_confirm_coverage(
                &self.inner.store,
                &created.session_id,
                committed.unwrap_or_default(),
                created.created_seq,
            )
            .await
            .map_err(fork_pipe_publication_error)?;
        let final_envelope = match committed.and_then(|envelopes| envelopes.last()).cloned() {
            Some(envelope) if envelope.seq == created.created_seq => envelope,
            _ => self
                .inner
                .store
                .read(
                    &created.session_id,
                    created.created_seq.saturating_sub(1),
                    1,
                )
                .await?
                .into_iter()
                .find(|envelope| envelope.seq == created.created_seq)
                .ok_or_else(|| {
                    SessionHubError::Store(HaiderError::new(
                        ErrorCode::StoreCorrupt,
                        "committed fork receipt has no matching final audit envelope",
                        false,
                    ))
                })?,
        };
        lock(&self.inner.fork_candidates)?.remove(&created.session_id);
        // Copied parent facts must not rerun hooks. Only the final fork audit
        // originated in this transaction; this is the same projection seam
        // every session actor's `publish` uses.
        self.inner
            .commit_projection
            .observe_committed(std::slice::from_ref(&final_envelope));
        self.inner.observer.observe(HubObservation::Published {
            session_id: created.session_id.clone(),
            through_seq: created.created_seq,
        });
        Ok(())
    }

    async fn publish_received_fork(
        &self,
        created: &CreatedSessionFork,
    ) -> Result<(), SessionHubError> {
        let _publication_serial = self.inner.fork_publication_serial.lock().await;
        self.publish_committed_fork(created, None).await
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

    async fn checkpoint_command_receipt(
        &self,
        command_id: &CommandId,
        method: &str,
        request_digest: &str,
        request_json: &str,
    ) -> Result<Option<haider_protocol::checkpoint::CheckpointMutationReceipt>, SessionHubError>
    {
        self.inner
            .store
            .checkpoint_command_receipt(
                command_id.0.clone(),
                method.to_owned(),
                request_digest.to_owned(),
                request_json.to_owned(),
            )
            .await
            .map_err(Into::into)
    }

    async fn commit_checkpoint(
        &self,
        command: CheckpointCommitCommand,
    ) -> Result<CheckpointCommitOutcome, CheckpointCommitFailure> {
        let actor = self
            .actor_for(command.session_id.clone())
            .await
            .map_err(CheckpointCommitFailure::DefinitelyUncommitted)?;
        let (completed, result) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::CommitCheckpoint { command, completed })
            .await
            .map_err(|_| CheckpointCommitFailure::DefinitelyUncommitted(SessionHubError::Closed))?;
        result
            .await
            .map_err(|_| CheckpointCommitFailure::Ambiguous(SessionHubError::Closed))?
            .map_err(|error| {
                CheckpointCommitFailure::DefinitelyUncommitted(SessionHubError::Store(error))
            })
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

    pub(crate) async fn workflow_graph_state(
        &self,
        session_id: &SessionId,
        graph_id: Option<haider_protocol::ids::GraphId>,
    ) -> Result<Option<haider_protocol::graph::WorkflowGraphState>, SessionHubError> {
        self.inner
            .store
            .workflow_graph_state(session_id, graph_id)
            .await
            .map_err(Into::into)
    }

    pub(crate) async fn workflow_graph_watch(
        &self,
        session_id: &SessionId,
        after_cursor: u64,
        limit: u32,
    ) -> Result<haider_protocol::graph::WorkflowGraphWatchPage, SessionHubError> {
        self.inner
            .store
            .workflow_graph_watch(session_id, after_cursor, limit)
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
            .send(ActorCommand::SwitchGraph {
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

    pub(crate) async fn switch_graph_matching_digest(
        &self,
        command: GraphSwitchCommand,
        expected_digest: String,
    ) -> Result<GraphSwitchOutcome, SessionHubError> {
        let actor = self.actor_for(command.session_id.clone()).await?;
        let (completed, result) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::SwitchGraph {
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
        self.accept_turn_with_auto_title(command, None).await
    }

    async fn accept_turn_with_auto_title(
        &self,
        command: TurnAcceptCommand,
        auto_title: Option<String>,
    ) -> Result<TurnAcceptOutcome, SessionHubError> {
        let actor = self.actor_for(command.session_id.clone()).await?;
        let _workflow_selection = self.lock_workflow_selection(&command.session_id).await;
        let (completed, result) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::AcceptTurn {
                command,
                peer_message: None,
                auto_title,
                completed,
            })
            .await
            .map_err(|_| SessionHubError::Closed)?;
        result
            .await
            .map_err(|_| SessionHubError::Closed)?
            .map_err(Into::into)
    }

    pub(crate) async fn queue_snapshot(
        &self,
        session_id: SessionId,
    ) -> Result<QueueSnapshot, SessionHubError> {
        let actor = self.actor_for(session_id).await?;
        let (completed, result) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::QueueList { completed })
            .await
            .map_err(|_| SessionHubError::Closed)?;
        result
            .await
            .map_err(|_| SessionHubError::Closed)?
            .map_err(Into::into)
    }

    async fn queue_remove(
        &self,
        command: QueueRemoveCommand,
    ) -> Result<QueueRemoveOutcome, SessionHubError> {
        let actor = self.actor_for(command.session_id.clone()).await?;
        let (completed, result) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::QueueRemove { command, completed })
            .await
            .map_err(|_| SessionHubError::Closed)?;
        result
            .await
            .map_err(|_| SessionHubError::Closed)?
            .map_err(Into::into)
    }

    pub(crate) async fn queue_promote_steer(
        &self,
        command: QueuePromoteCommand,
    ) -> Result<(QueuePromoteOutcome, bool), SessionHubError> {
        let actor = self.actor_for(command.session_id.clone()).await?;
        let (completed, result) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::QueuePromoteSteer { command, completed })
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
        let _workflow_selection = self.lock_workflow_selection(&command.session_id).await;
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
        let _workflow_selection = self.lock_workflow_selection(&command.session_id).await;
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

    pub(crate) async fn acquire_worker_lease_with_wakes(
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

    /// Return true only when every durable run in the profile is terminal.
    ///
    /// Auto-spawn retirement calls this after the last client disconnects.
    /// The journal remains the authority: resident-worker count and volatile
    /// actor state are deliberately insufficient for a shutdown decision.
    pub(crate) async fn daemon_is_durably_quiescent(&self) -> Result<bool, SessionHubError> {
        for session_id in self.session_ids().await? {
            if self.session_has_nonterminal_runs(&session_id).await? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn roster_session_ids(&self) -> Result<Vec<SessionId>, SessionHubError> {
        let mut session_ids = self.inner.store.session_ids().await?;
        let fork_candidates = lock(&self.inner.fork_candidates)?;
        session_ids.retain(|session_id| !fork_candidates.contains(session_id));
        Ok(session_ids)
    }

    async fn roster_session_count(&self) -> Result<u64, SessionHubError> {
        let candidates = lock(&self.inner.fork_candidates)?
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        let total = self.inner.store.session_count().await?;
        let mut hidden = 0_u64;
        for candidate in candidates {
            if self
                .inner
                .store
                .session_metadata(&candidate)
                .await?
                .is_some()
            {
                hidden = hidden.saturating_add(1);
            }
        }
        Ok(total.saturating_sub(hidden))
    }

    fn is_roster_visible(&self, session_id: &SessionId) -> Result<bool, SessionHubError> {
        Ok(!lock(&self.inner.fork_candidates)?.contains(session_id))
    }

    /// Deletes one quiesced session through the daemon's production
    /// lifecycle. New actor admission is fenced first; attached or
    /// nonterminal sessions are refused. The actor stops before the durable
    /// transaction, and ephemeral handoff data is cleaned only after commit.
    pub async fn delete_session(&self, session_id: SessionId) -> Result<(), HaiderError> {
        // Preserve the old immediate refusal before waiting on any unrelated
        // hook backlog. Eligibility is checked again under the tombstone.
        let _ = self.deletion_metadata_if_eligible(&session_id).await?;
        let (hooks_were_installed, hooks) = {
            let installed = lock(&self.inner.hooks).map_err(hub_error_as_store)?;
            (
                installed.is_some(),
                installed
                    .as_ref()
                    .and_then(crate::hooks::WeakHookService::upgrade),
            )
        };
        if hooks_were_installed
            && hooks.is_none()
            && self
                .inner
                .store
                .has_pending_hook_dispatches(&session_id)
                .await?
        {
            return Err(HaiderError::new(
                ErrorCode::Busy,
                "hook engine is unavailable with dispatch pending; retry session deletion",
                true,
            ));
        }
        if let Some(hooks) = &hooks {
            hooks
                .drain_session_before_delete(session_id.clone())
                .await?;
        }
        // Peer delivery appends its shared `Claimed` marker while holding this
        // same serial. Deletion must wait until the claim either commits its
        // core run or fails before publishing a deletion tombstone.
        let workflow_selection = self.lock_workflow_selection(&session_id).await;
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
        drop(workflow_selection);
        let result = self
            .delete_fenced_session(&session_id, hooks_were_installed, hooks.as_ref())
            .await;
        if result.is_err()
            && let Ok(mut deleting) = lock(&self.inner.deleting_sessions)
        {
            deleting.remove(&session_id);
        }
        result
    }

    async fn deletion_metadata_if_eligible(
        &self,
        session_id: &SessionId,
    ) -> Result<haider_protocol::session::SessionMetadataV1, HaiderError> {
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
            || lock(&self.inner.descendant_attachments)
                .map_err(hub_error_as_store)?
                .values()
                .any(|owner| owner.session_ids.contains(session_id))
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
        Ok(metadata)
    }

    async fn delete_fenced_session(
        &self,
        session_id: &SessionId,
        hooks_were_installed: bool,
        hooks: Option<&crate::hooks::HookService>,
    ) -> Result<(), HaiderError> {
        let metadata = self.deletion_metadata_if_eligible(session_id).await?;
        let actor = lock(&self.inner.actors)
            .map_err(hub_error_as_store)?
            .get(session_id)
            .cloned();
        if let Some(actor) = actor.as_ref() {
            // First consume every command admitted before the permanent
            // deletion tombstone and prove the durable session is quiescent.
            // Keep the actor alive so the worker lease can be unregistered
            // through the actor's FIFO before its supervisor joins.
            let (completed, quiescent) = oneshot::channel();
            actor
                .commands
                .send(ActorCommand::FenceIfQuiescent { completed })
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
        }
        let worker_manager = lock(&self.inner.worker_manager)
            .map_err(hub_error_as_store)?
            .clone();
        if let Some(worker_manager) = worker_manager {
            // The FIFO deletion fence and permanent admission tombstone are
            // both installed. Close the supervisor, await its acknowledged
            // lease unregister, then wait for the manager-owned JoinSet to
            // join and remove the slot before stopping the actor.
            worker_manager.retire(session_id.clone()).await?;
        }
        // Publish the target-unavailable terminal state before deleting the
        // private core record. The deletion tombstone already blocks a new
        // claim, so a crash after the store delete cannot leave a foreign
        // scanner free to reinterpret an unresolved shared claim.
        let peer_service = lock(&self.inner.peer_service)
            .map_err(hub_error_as_store)?
            .clone();
        if let Some(peer_service) = peer_service {
            peer_service
                .expire_target(
                    session_id.as_str(),
                    haider_protocol::peer::PeerDeliveryReason::TargetUnavailable,
                )
                .await
                .map_err(|error| {
                    HaiderError::new(
                        ErrorCode::Internal,
                        format!("peer mailbox deletion fence failed: {error}"),
                        true,
                    )
                })?;
        }
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
            // Remove both live registries while the deletion tombstone still
            // fences recreation, then join without holding either mutex.
            lock(&self.inner.actors)
                .map_err(hub_error_as_store)?
                .remove(session_id);
            let actor_task = lock(&self.inner.session_actor_tasks)
                .map_err(hub_error_as_store)?
                .remove(session_id)
                .ok_or_else(|| {
                    HaiderError::new(
                        ErrorCode::Internal,
                        "session actor task was missing during deletion fencing",
                        true,
                    )
                })?;
            actor_task.await.map_err(|error| {
                HaiderError::new(
                    ErrorCode::Internal,
                    format!("session actor failed during deletion fencing: {error}"),
                    true,
                )
            })?;
            // The actor stopped only after its FIFO-local quiescence check.
            // The deletion tombstone prevents any replacement from racing
            // this removal.
        }
        // The pre-tombstone hook drain and this post-actor check form one
        // deletion fence. Any append admitted between them either precedes
        // StopIfQuiescent in the actor FIFO and leaves a durable outbox row,
        // or is rejected by the deletion tombstone. Never erase an outbox row
        // merely because its payload-carrying live queue became a wake.
        if hooks_were_installed
            && self
                .inner
                .store
                .has_pending_hook_dispatches(session_id)
                .await?
        {
            return Err(HaiderError::new(
                ErrorCode::Busy,
                "hook dispatch remains pending; retry session deletion",
                true,
            ));
        }
        // W-A fence law: session close kills every pgid the session owns,
        // after the actor is provably stopped and before the durable delete.
        self.fence_background_tasks(session_id).await;
        self.inner
            .monitors
            .forget_session(self, session_id)
            .await
            .map_err(|error| {
                HaiderError::new(
                    ErrorCode::Internal,
                    format!("monitor deletion fence failed: {error}"),
                    true,
                )
            })?;
        match self.inner.store.delete_session(session_id.clone()).await {
            Ok(()) => self.inner.monitors.release_session_tombstone(session_id),
            Err(error) => {
                self.inner
                    .monitors
                    .restore_session(self, session_id)
                    .await
                    .map_err(|restore| {
                        HaiderError::new(
                            ErrorCode::Internal,
                            format!(
                                "session delete failed ({error}); monitor rollback failed: {restore}"
                            ),
                            true,
                        )
                    })?;
                return Err(error);
            }
        }
        self.inner.pipe_native.release_clean(session_id);
        self.inner
            .workflow_selection_serials
            .lock()
            .await
            .remove(session_id);
        self.inner
            .checkpoint_serials
            .lock()
            .await
            .remove(session_id);
        self.inner
            .web_degrade
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session_id);
        self.inner.observe_digests.remove(session_id);
        self.inner
            .ssh_scopes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session_id);
        self.inner
            .lockdown_turns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|(candidate, _), _| candidate != session_id);
        if let Ok(manager) = crate::lockdown::global() {
            let profile_id = self.inner.store.cached_profile_installation_id();
            if let Err(error) = manager.remove_session_bindings(profile_id, session_id.as_str()) {
                tracing::warn!(
                    session_id = %session_id,
                    ?error,
                    "durable session deletion committed but lockdown shell cleanup failed"
                );
            }
        }
        if let Some(hooks) = hooks {
            hooks.session_deleted(session_id.clone()).await;
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

    /// One per-session total order for native workflow selection and the
    /// durable admission of work that makes an idle session nonterminal.
    pub(crate) async fn lock_workflow_selection(
        &self,
        session_id: &SessionId,
    ) -> tokio::sync::OwnedMutexGuard<()> {
        let serial = {
            let mut serials = self.inner.workflow_selection_serials.lock().await;
            Arc::clone(
                serials
                    .entry(session_id.clone())
                    .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
            )
        };
        serial.lock_owned().await
    }

    pub(crate) async fn lock_checkpoint_mutation(
        &self,
        session_id: &SessionId,
    ) -> tokio::sync::OwnedMutexGuard<()> {
        let serial = {
            let mut serials = self.inner.checkpoint_serials.lock().await;
            Arc::clone(
                serials
                    .entry(session_id.clone())
                    .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
            )
        };
        serial.lock_owned().await
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
        self.actor_for_inner(session_id, false).await
    }

    async fn actor_for_inner(
        &self,
        session_id: SessionId,
        allow_committed_fork_candidate: bool,
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
        if !allow_committed_fork_candidate
            && lock(&self.inner.fork_candidates)?.contains(&session_id)
        {
            return Err(SessionHubError::Task(
                "session fork candidate is not yet committed".into(),
            ));
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
        if !allow_committed_fork_candidate
            && lock(&self.inner.fork_candidates)?.contains(&session_id)
        {
            return Err(SessionHubError::Task(
                "session fork candidate is not yet committed".into(),
            ));
        }
        if let Some(actor) = actors.get(&session_id) {
            return Ok(actor.clone());
        }
        let authority_epoch = last.as_ref().map_or(0, |envelope| envelope.authority_epoch);
        let (commands, receiver) = mpsc::channel(self.inner.config.actor_command_capacity);
        let actor = SessionActorHandle { commands };
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
        lock(&self.inner.session_actor_tasks)?.insert(session_id.clone(), task);
        actors.insert(session_id.clone(), actor.clone());
        let _ = self.inner.roster_publications.send(session_id);
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

    fn register_descendant_attachment(
        &self,
        connection_id: &str,
        root_session_id: SessionId,
        mut session_ids: HashSet<SessionId>,
    ) -> Result<DescendantRegisterResult, SessionHubError> {
        // Serialize final publication with the same permanent deletion fence
        // used by ordinary session attachments. If registration wins, the
        // deleter observes this owner; if deletion wins, no stream is minted.
        let deleting = lock(&self.inner.deleting_sessions)?;
        session_ids.insert(root_session_id);
        if session_ids
            .iter()
            .any(|session_id| deleting.contains(session_id))
        {
            return Ok(DescendantRegisterResult::SessionUnavailable);
        }
        if let Some(message) = self.reserve_attachment_slot(connection_id)? {
            return Ok(DescendantRegisterResult::Overloaded { message });
        }
        let slot = AttachmentSlotGuard {
            hub: self.clone(),
            connection_id: connection_id.to_owned(),
            armed: true,
        };
        let attachment_id = AttachmentId::new(random_id("descendants")?);
        let (cancel, cancel_receiver) = watch::channel(false);
        lock(&self.inner.descendant_attachments)?.insert(
            attachment_id.clone(),
            DescendantAttachmentOwner {
                connection_id: connection_id.to_owned(),
                session_ids,
                cancel,
            },
        );
        drop(deleting);
        slot.transfer();
        Ok(DescendantRegisterResult::Registered {
            attachment_id,
            cancel: cancel_receiver,
        })
    }

    fn track_descendant_attachment_session(
        &self,
        attachment_id: &AttachmentId,
        session_id: SessionId,
    ) -> Result<bool, SessionHubError> {
        // Keep live cohort growth on the same side of the deletion fence as
        // initial registration. A successful insert makes deletion observe
        // this attachment; a pre-existing deletion tombstone refuses it.
        let deleting = lock(&self.inner.deleting_sessions)?;
        if deleting.contains(&session_id) {
            return Ok(false);
        }
        let mut attachments = lock(&self.inner.descendant_attachments)?;
        let Some(owner) = attachments.get_mut(attachment_id) else {
            return Ok(false);
        };
        owner.session_ids.insert(session_id);
        drop(attachments);
        drop(deleting);
        Ok(true)
    }

    fn spawn_descendant_stream(
        &self,
        attachment_id: AttachmentId,
        prepared: descendant_stream::PreparedDescendantStream,
        sink: Arc<dyn FrameSink>,
        cancel: watch::Receiver<bool>,
    ) -> Result<(), SessionHubError> {
        if self.inner.draining.load(Ordering::Acquire) {
            return Err(SessionHubError::Closed);
        }
        let mut replay_tasks = lock(&self.inner.replay_tasks)?;
        if self.inner.draining.load(Ordering::Acquire) {
            return Err(SessionHubError::Closed);
        }
        replay_tasks.retain(|handle| !handle.is_finished());
        replay_tasks.push(tokio::spawn(run_descendant_stream(
            self.clone(),
            attachment_id,
            prepared,
            sink,
            cancel,
        )));
        Ok(())
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

    fn take_descendant_attachment(
        &self,
        attachment_id: &AttachmentId,
        connection_id: Option<&str>,
    ) -> Result<Option<DescendantAttachmentOwner>, SessionHubError> {
        let mut attachments = lock(&self.inner.descendant_attachments)?;
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

    fn detach_descendant(&self, attachment_id: &AttachmentId) -> Result<bool, SessionHubError> {
        Ok(self
            .take_descendant_attachment(attachment_id, None)?
            .is_some())
    }

    fn repair_and_detach_descendant(
        &self,
        sink: &Arc<dyn FrameSink>,
        attachment_id: &AttachmentId,
        children: Vec<haider_rpc::DescendantIdentityWire>,
    ) {
        let Ok(Some(_owner)) = self.take_descendant_attachment(attachment_id, None) else {
            return;
        };
        self.inner
            .metrics
            .outbox_detaches
            .fetch_add(1, Ordering::Relaxed);
        match sink.purge_attachment(attachment_id) {
            Some(request_id) => {
                if sink
                    .try_send(WireFrame::Response {
                        request_id,
                        body: ResponseBody::Error {
                            code: haider_rpc::ERROR_CODE_OVERLOADED.into(),
                            message: "descendant attachment could not start authoritatively; re-attach from your applied per-child cursors".into(),
                            retryable: true,
                            data: None,
                        },
                    })
                    .is_err()
                {
                    sink.close_after_required_delivery_failure();
                }
            }
            None => {
                if sink
                    .try_send(WireFrame::SessionDescendantRepairRequired {
                        attachment_id: attachment_id.clone(),
                        children,
                    })
                    .is_err()
                {
                    sink.close_after_required_delivery_failure();
                }
            }
        }
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
        let deleting = self
            .inner
            .deleting_sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(session_id);
        let surfaces = self
            .inner
            .surfaces
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = surfaces.get(session_id);
        let change_generation = state.map_or_else(
            || if deleting { u64::MAX } else { 0 },
            |state| state.change_generation,
        );
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
        surfaces.remove(session_id);
        drop(surfaces);
        // The permanent deletion tombstone supplies `u64::MAX` as the one
        // cleared terminal generation, so watchers still observe deletion
        // while this non-authoritative shell is released.
        self.notify_surface_watchers();
    }

    fn detach_connection_registrations(
        &self,
        connection_id: &str,
    ) -> Result<Vec<(AttachmentId, AttachmentOwner)>, SessionHubError> {
        lock(&self.inner.resident_binding_viewers)?.remove(connection_id);
        lock(&self.inner.peer_event_subscribers)?.remove(connection_id);
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
        let descendant_ids = {
            let owners = lock(&self.inner.descendant_attachments)?;
            owners
                .iter()
                .filter(|(_, owner)| owner.connection_id == connection_id)
                .map(|(attachment_id, _)| attachment_id.clone())
                .collect::<Vec<_>>()
        };
        for attachment_id in descendant_ids {
            let _ = self.take_descendant_attachment(&attachment_id, Some(connection_id))?;
        }
        Ok(attachments)
    }

    async fn detach_connection(&self, connection_id: &str) -> Result<(), SessionHubError> {
        let attachments = self.detach_connection_registrations(connection_id)?;
        self.inner
            .shells
            .close_owner(connection_id)
            .map_err(|error| SessionHubError::Task(error.to_string()))?;
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
        if attachments.contains_key(attachment_id) {
            return sink.offer_prepared(attachment_id, frame);
        }
        drop(attachments);
        let Ok(descendants) = lock(&self.inner.descendant_attachments) else {
            return SendAdmission::Refused;
        };
        if !descendants.contains_key(attachment_id) {
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
        if attachments.contains_key(attachment_id) {
            return sink.offer_prepared_ticketed(attachment_id, frame, ticket);
        }
        drop(attachments);
        let Ok(descendants) = lock(&self.inner.descendant_attachments) else {
            return SendAdmission::Refused;
        };
        if !descendants.contains_key(attachment_id) {
            return SendAdmission::Refused;
        }
        // Keep the same admit-vs-detach ownership barrier as the fresh offer.
        sink.offer_prepared_ticketed(attachment_id, frame, ticket)
    }

    /// Rejects new hub work synchronously before the runtime announces drain.
    pub fn begin_draining(&self) {
        self.inner.draining.store(true, Ordering::Release);
        let _ = self.inner.shell_registry_events_cancel.send(true);
        if let Ok(peer) = self.peer_service() {
            peer.begin_draining();
        }
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
        self.inner
            .monitors
            .shutdown()
            .await
            .map_err(|error| SessionHubError::Task(error.to_string()))?;
        // W-A fence law: background tasks die with the daemon. The kill
        // ladders run BEFORE the drain flag so completion facts can still
        // journal; anything unsettled is reaped by next-start adoption.
        self.shutdown_background_tasks().await;
        self.begin_draining();
        if let Ok(peer) = self.peer_service() {
            peer.shutdown().await;
        }
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
        let session_actor_tasks = std::mem::take(&mut *lock(&self.inner.session_actor_tasks)?)
            .into_values()
            .collect::<Vec<_>>();
        let mut session_actor_tasks =
            OwnedTasks::new(session_actor_tasks, Arc::clone(&self.inner.force_stop));
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
        let _ = session_actor_tasks.join_all().await;
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
        let descendant_owners = {
            let mut owners = lock(&self.inner.descendant_attachments)?;
            owners.drain().collect::<Vec<_>>()
        };
        // The drained owners bypass `take_attachment`; clear their admission
        // ledger wholesale (no new reservation is admitted while draining).
        *lock(&self.inner.attachment_slots)? = AttachmentSlots::default();
        for (_, owner) in &owners {
            let _ = owner.cancel.send(true);
        }
        for (_, owner) in &descendant_owners {
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

pub(super) fn peer_event_allowed(subscribers: &HashSet<String>, connection_id: &str) -> bool {
    subscribers.contains(connection_id)
}

/// The worker-facing store surface. Reads go straight to the store: committed
/// history needs no actor serialization.
#[async_trait]
impl StoreHandle for HubStoreHandle {
    async fn append(
        &self,
        envelopes: &mut [RawEnvelope],
    ) -> Result<haider_core::CommittedRange, HaiderError> {
        let committed = self.append_owned_inner(None, envelopes.to_vec()).await?;
        envelopes.clone_from_slice(&committed);
        Ok(haider_core::CommittedRange {
            first_seq: committed.first().map_or(0, |envelope| envelope.seq),
            last_seq: committed.last().map_or(0, |envelope| envelope.seq),
        })
    }

    async fn append_owned(
        &self,
        envelopes: Vec<RawEnvelope>,
    ) -> Result<Arc<[RawEnvelope]>, HaiderError> {
        self.append_owned_inner(None, envelopes).await
    }

    async fn persist_context_economy(
        &self,
        session_id: &SessionId,
        economy: &haider_protocol::context::ContextEconomy,
    ) -> Result<(), HaiderError> {
        self.ensure_session(session_id)?;
        self.hub
            .inner
            .store
            .persist_context_economy(session_id.clone(), economy.clone())
            .await
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

    async fn read_reducer_page(
        &self,
        session_id: &SessionId,
        since_seq: u64,
        limit: usize,
        byte_budget: usize,
        payload_kinds: &'static [&'static str],
    ) -> Result<Vec<RawEnvelope>, HaiderError> {
        self.ensure_session(session_id)?;
        self.hub
            .inner
            .store
            .read_reducer_page(session_id, since_seq, limit, byte_budget, payload_kinds)
            .await
    }

    async fn read_reducer_page_with_boundary(
        &self,
        session_id: &SessionId,
        since_seq: u64,
        limit: usize,
        byte_budget: usize,
        payload_kinds: &'static [&'static str],
    ) -> Result<haider_core::ReducerPage, HaiderError> {
        self.ensure_session(session_id)?;
        self.hub
            .inner
            .store
            .read_reducer_page_with_boundary(
                session_id,
                since_seq,
                limit,
                byte_budget,
                payload_kinds,
            )
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

    async fn persist_provider_view(
        &self,
        session_id: &SessionId,
        ledger: haider_protocol::cache::ProviderViewLedgerV1,
        blobs: Vec<haider_protocol::cache::ProviderViewBlobV1>,
    ) -> Result<haider_protocol::cache::ProviderViewLedgerV1, HaiderError> {
        self.ensure_session(session_id)?;
        self.hub
            .inner
            .store
            .persist_provider_view(session_id.clone(), ledger, blobs)
            .await
    }

    async fn persist_provider_view_and_append_owned(
        &self,
        request: ProviderViewAppendRequest,
    ) -> Result<ProviderViewAppendOutcome, HaiderError> {
        if request.session_id != self.session_id
            || request.envelopes.is_empty()
            || request.envelopes.iter().any(|envelope| {
                envelope.session_id != self.session_id
                    || envelope.worker_generation != self.worker_generation
            })
        {
            return Err(HaiderError::new(
                ErrorCode::SingleWriterViolation,
                "provider-view append identity does not match its worker lease",
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
            .send(ActorCommand::WorkerProviderViewAppend {
                lease_id: self.lease_id.clone(),
                request,
                completed,
            })
            .await
            .map_err(|_| hub_closed_store_error())?;
        response.await.map_err(|_| hub_closed_store_error())?
    }

    async fn verify_provider_view(
        &self,
        ledger: &haider_protocol::cache::ProviderViewLedgerV1,
    ) -> Result<(), HaiderError> {
        self.hub
            .inner
            .store
            .verify_provider_view(ledger.clone())
            .await
    }

    async fn read_provider_view_block(
        &self,
        ledger: &haider_protocol::cache::ProviderViewLedgerV1,
        block: &haider_protocol::cache::ProviderViewBlockRefV1,
    ) -> Result<Vec<u8>, HaiderError> {
        self.hub
            .inner
            .store
            .read_provider_view_block(ledger.clone(), block.clone())
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
    async fn append_owned_inner(
        &self,
        expected_head: Option<u64>,
        envelopes: Vec<RawEnvelope>,
    ) -> Result<Arc<[RawEnvelope]>, HaiderError> {
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
        if inject_test_done_append_failure(&envelopes) {
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
                expected_head,
                envelopes,
                completed,
            })
            .await
            .map_err(|_| hub_closed_store_error())?;
        response.await.map_err(|_| hub_closed_store_error())?
    }

    pub(crate) async fn consume_queued_turn(
        &self,
        run_id: RunId,
        delta_event_id: EventId,
        device_id: DeviceId,
    ) -> Result<Option<QueueConsumeOutcome>, HaiderError> {
        let actor = self
            .hub
            .existing_actor(&self.session_id)
            .map_err(hub_error_as_store)?
            .ok_or_else(hub_closed_store_error)?;
        let (completed, result) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::QueueConsume {
                lease_id: self.lease_id.clone(),
                command: QueueConsumeCommand {
                    session_id: self.session_id.clone(),
                    run_id,
                    delta_event_id,
                    device_id,
                },
                completed,
            })
            .await
            .map_err(|_| hub_closed_store_error())?;
        result.await.map_err(|_| hub_closed_store_error())?
    }

    /// Appends a worker batch only while the session journal still has the
    /// exact head observed by the caller. The session actor performs the
    /// comparison and append without yielding to another session command.
    pub(crate) async fn append_at_head(
        &self,
        expected_head: u64,
        envelopes: &mut [RawEnvelope],
    ) -> Result<haider_core::CommittedRange, HaiderError> {
        let committed = self
            .append_owned_inner(Some(expected_head), envelopes.to_vec())
            .await?;
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

    pub(crate) fn turn_setup_reduction_cache(&self) -> &TurnSetupReductionCache {
        &self.hub.inner.turn_setup_reductions
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
        let (completed, response) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::UnregisterHarness {
                lease_id: self.lease_id.clone(),
                completed,
            })
            .await
            .map_err(|_| SessionHubError::Closed)?;
        response.await.map_err(|_| SessionHubError::Closed)
    }

    /// Stores opaque artifact bytes through this worker lease's bounded CAS
    /// surface. Public tool factories use this to back checkpoint journals
    /// without gaining access to the hub's underlying cross-session store.
    pub async fn put_artifact(
        &self,
        bytes: Vec<u8>,
    ) -> Result<haider_protocol::ids::ArtifactRef, HaiderError> {
        self.hub.inner.store.put(bytes).await
    }

    pub(crate) async fn put_artifact_batch(
        &self,
        blobs: Vec<Vec<u8>>,
    ) -> Result<Vec<haider_protocol::ids::ArtifactRef>, HaiderError> {
        self.hub.inner.store.put_batch(blobs).await
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
        let settled = response.await.map_err(|_| hub_closed_store_error())??;
        if let Some(envelope) = &settled {
            self.hub
                .trace_retention_snapshot(&self.session_id, envelope.seq, "idle")
                .await;
            let compacted_prompt_bytes = self
                .hub
                .inner
                .prompt_history
                .compact_session_history(&self.session_id)
                .await;
            if retention_trace_enabled() {
                self.hub
                    .trace_retention_snapshot(&self.session_id, envelope.seq, "compacted")
                    .await;
            }
            // Each physical request briefly materializes the growing prompt
            // and provider request. Compact the journal-derived prompt state
            // first, then scavenge the pages it released at this exact idle
            // head instead of carrying their high-water into the next turn.
            let allocator_bytes = haider_platform::allocator_pressure_relief();
            tracing::debug!(
                session_id = %self.session_id,
                idle_seq = envelope.seq,
                compacted_prompt_bytes,
                allocator_bytes,
                "compacted prompt history at terminal idle boundary"
            );
            self.hub
                .schedule_idle_derived_state_release(self.session_id.clone(), envelope.seq);
        }
        Ok(settled)
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

fn peer_turn_coordinates(
    message: &haider_protocol::peer::PeerMessage,
) -> Result<(String, String, String), SessionHubError> {
    let request_json = serde_json::to_string(message).map_err(|error| {
        SessionHubError::Task(format!("cannot encode peer delivery coordinates: {error}"))
    })?;
    let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
    Ok((
        format!("peer:{}", message.msg_id),
        request_digest,
        request_json,
    ))
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
