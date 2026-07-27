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
//!   the daemon on a slow client (R12's mechanism): exceeding either bound is
//!   a connection error, and the store — not the socket — is the lag buffer in
//!   later lanes;
//! - one `ServerDraining` is the last frame of THIS LANE's traffic on a
//!   draining connection (R17), and it travels a reserved path so ordinary
//!   replies can never spend the queue slot or the bytes that last frame
//!   needs. Once the notice is reserved, EVERY write is bounded by the drain
//!   deadline, so the last frame can neither be starved behind an in-flight
//!   ordinary write nor outlive the barrier. SCOPE, for W3b2: the binding
//!   spec closes transports only AFTER the final committed envelopes are
//!   broadcast during the grace period (d1 report §6.6 step 10), so once
//!   attachments stream through this barrier the rule becomes "notice, then
//!   keep streaming until checkpoint or deadline" — a deliberate relaxation,
//!   not a regression, and it must keep the deadline discipline intact;
//! - a peer that never finishes its handshake is closed at
//!   `handshake_timeout`, so silent peers cannot hold connection slots;
//! - a peer accepted beyond the daemon's connection cap is answered with a
//!   fatal `overloaded` error and closed without ever entering this layer's
//!   task/queue accounting (report §2.5).
//!
//! W3b2 seams: `Request` bodies and `MenuAnswer` are answered with typed
//! `draining` / `not_found` stubs (see [`handle_frame`] / [`enqueue_stub`]);
//! session hub, attach/replay, and menu arbitration replace them in W3b2. The
//! negotiated grant ([`ConnectionGrant`]) is retained for that authorization.

use crate::DaemonError;
use haider_rpc::{
    Capability, CapabilitySet, ERROR_CODE_CAPABILITY_DENIED, ERROR_CODE_DRAINING,
    ERROR_CODE_NOT_FOUND, ERROR_CODE_OVERLOADED, ErrorData, Hello, LifecyclePhase, ProtocolError,
    RequestId, ResponseBody, ServerRange, Welcome, WireFrame, negotiate, uds_codec,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;

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
}

impl Drop for WriterGuard {
    fn drop(&mut self) {
        // Best effort, and a no-op once the writer has finished: a connection
        // that ends early must not leave its writer parked on a peer that
        // never reads. The runtime still joins the task.
        self.abort.abort();
    }
}

/// The reserved last frame plus the barrier deadline that bounds every write
/// from the moment it exists (R17).
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
    ClosedBeforeDrain,
    /// `ServerDraining` was written and the socket was shut down cleanly.
    NoticeDelivered,
    /// The daemon could not deliver `ServerDraining`: the negotiated frame
    /// limit refused even a truncated notice, or the barrier deadline expired
    /// while this connection was still writing.
    NoticeUndelivered,
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
}

impl ConnectionGrant {
    fn grants(&self, capability: Capability) -> bool {
        self.capabilities.contains(&capability)
    }
}

/// Per-connection ledger of encoded bytes queued but not yet written.
///
/// The frame-count queue bounds how MANY replies may be pending; this bounds
/// how much memory those replies may hold, so `outbound_queue_capacity ×
/// frame_limit` never becomes the real bound. Bytes are charged before enqueue
/// and credited only once the write completes — an in-flight frame still owns
/// its allocation.
struct QueuedBytes {
    queued: AtomicUsize,
    budget: usize,
}

impl QueuedBytes {
    fn new(budget: usize) -> Self {
        Self {
            queued: AtomicUsize::new(0),
            budget,
        }
    }

    /// Reserves `bytes`, or reports the currently queued total without
    /// reserving anything. Never partially charges.
    fn charge(&self, bytes: usize) -> Result<(), usize> {
        self.queued
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                queued
                    .checked_add(bytes)
                    .filter(|total| *total <= self.budget)
            })
            .map(|_| ())
    }

    /// Returns `bytes` to the budget. Saturating by construction: a credit may
    /// never invent capacity by wrapping below zero.
    fn credit(&self, bytes: usize) {
        let _ = self
            .queued
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                Some(queued.saturating_sub(bytes))
            });
    }
}

/// The ordinary outbound lane: the bounded frame queue plus its byte ledger.
///
/// The final `ServerDraining` frame deliberately does NOT travel here; see the
/// reserved drain path in [`serve`].
struct OutboundLane {
    frames: mpsc::Sender<Vec<u8>>,
    queued_bytes: Arc<QueuedBytes>,
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
#[derive(Debug, Clone)]
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
    /// For error context only; the stream is already accepted.
    pub(crate) endpoint_path: PathBuf,
}

/// Runs one client connection to completion: UID gate, framed read loop,
/// bounded write queue, and a final `ServerDraining` + close when the drain
/// broadcast fires. Errors returned here end only this connection; the
/// [`ConnectionExit`] is what the drain barrier reads to stay honest.
pub(crate) async fn serve(
    stream: UnixStream,
    context: ConnectionContext,
    mut drain: watch::Receiver<Option<DrainNotice>>,
) -> Result<ConnectionExit, DaemonError> {
    let credentials = stream.peer_cred().map_err(|error| {
        DaemonError::io("read Unix peer credentials", &context.endpoint_path, error)
    })?;
    if credentials.uid() != context.owner_uid {
        return Err(DaemonError::Protocol {
            message: format!(
                "refusing peer uid {}, endpoint owner is {}",
                credentials.uid(),
                context.owner_uid
            ),
        });
    }

    let (mut reader, writer) = stream.into_split();
    let (outbound, queued) = mpsc::channel::<Vec<u8>>(context.outbound_queue_capacity);
    let lane = OutboundLane {
        frames: outbound,
        queued_bytes: Arc::new(QueuedBytes::new(context.outbound_queued_bytes)),
    };
    // Reserved drain path (R17): one dedicated slot, outside the ordinary
    // queue and outside its byte budget, so no volume of ordinary replies can
    // consume what the last frame needs.
    let (reserve, reserved) = mpsc::channel::<ReservedNotice>(1);
    let ledger = Arc::clone(&lane.queued_bytes);
    let (report, reported) = oneshot::channel::<std::io::Result<bool>>();
    let writer_handle = tokio::spawn(async move {
        let outcome = run_writer(writer, queued, reserved, ledger).await;
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

    let mut decoder = uds_codec::Decoder::new(context.frame_limit);
    let mut buffer = [0_u8; 16 * 1024];
    let mut grant = Option::<ConnectionGrant>::None;
    let mut outbound_limit = context.frame_limit;
    let mut close = false;
    let mut notice_reserved = false;
    let mut notice_refused = false;
    // A peer that never speaks must not hold its connection slot forever.
    let handshake_deadline = tokio::time::sleep(context.handshake_timeout);
    tokio::pin!(handshake_deadline);

    while !close {
        tokio::select! {
            changed = drain.changed() => {
                let notice = changed.is_ok().then(|| drain.borrow().clone()).flatten();
                if let Some(notice) = notice {
                    match encode_drain_notice(&notice, outbound_limit) {
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
                );
                close = true;
            }
            read = reader.read(&mut buffer) => {
                let read = read.map_err(|error| {
                    DaemonError::io("read Unix connection", &context.endpoint_path, error)
                })?;
                if read == 0 {
                    break;
                }
                let batch = decoder.push(&buffer[..read]);
                for frame in batch.frames {
                    close = handle_frame(
                        frame,
                        &context,
                        &drain,
                        &lane,
                        &mut grant,
                        &mut outbound_limit,
                    )?;
                    if close {
                        break;
                    }
                }
                if !close
                    && let Some(error) = batch.error
                {
                    let protocol_error = ProtocolError {
                        code: "invalid_frame".into(),
                        message: error.to_string(),
                        fatal: true,
                    };
                    let _ = enqueue(
                        &lane,
                        &WireFrame::ProtocolError(protocol_error),
                        outbound_limit,
                    );
                    close = true;
                }
            }
        }
    }

    // Closing both lanes lets the writer flush what is already queued and then
    // stop; the reserve close is what releases its wait when no notice exists.
    drop(lane);
    drop(reserve);
    let written = writer_task
        .finish()
        .await
        .map_err(|error| DaemonError::io("write Unix connection", &context.endpoint_path, error))?;
    Ok(match (notice_reserved || notice_refused, written) {
        (false, _) => ConnectionExit::ClosedBeforeDrain,
        (true, true) => ConnectionExit::NoticeDelivered,
        (true, false) => ConnectionExit::NoticeUndelivered,
    })
}

/// Writer half of one connection.
///
/// Ordinary frames are written in queue order and their bytes credited back to
/// the connection's budget as each write completes. The reserved drain notice
/// is accepted at any time — including while an ordinary frame is mid-write —
/// and from that moment every write is bounded by the barrier deadline, so the
/// last frame is neither starved behind ordinary work nor able to outlive the
/// drain. A deadline hit ends the connection rather than splicing the notice
/// into a truncated frame. Returns whether `ServerDraining` reached the wire.
async fn run_writer(
    mut writer: OwnedWriteHalf,
    mut queued: mpsc::Receiver<Vec<u8>>,
    mut reserved: mpsc::Receiver<ReservedNotice>,
    queued_bytes: Arc<QueuedBytes>,
) -> std::io::Result<bool> {
    let mut notice = Option::<ReservedNotice>::None;
    let mut reserve_open = true;
    loop {
        let next = if reserve_open {
            tokio::select! {
                frame = queued.recv() => frame,
                received = reserved.recv() => {
                    reserve_open = false;
                    notice = received;
                    continue;
                }
            }
        } else {
            queued.recv().await
        };
        let Some(bytes) = next else { break };
        let charged = bytes.len();
        let result = write_ordinary(
            &mut writer,
            &bytes,
            &mut notice,
            &mut reserve_open,
            &mut reserved,
        )
        .await;
        // Credit after the write settles: an in-flight frame still owns its
        // bytes, which is what makes the budget a real memory bound.
        queued_bytes.credit(charged);
        match result {
            Ok(()) => {}
            // The barrier expired mid-frame: stop, and report the notice as
            // undelivered rather than corrupt the stream with a partial frame.
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => return Ok(false),
            Err(error) => return Err(error),
        }
    }
    let Some(notice) = notice.take().or(reserved.recv().await) else {
        writer.shutdown().await?;
        return Ok(false);
    };
    match tokio::time::timeout_at(notice.deadline, writer.write_all(&notice.bytes)).await {
        Ok(Ok(())) => {
            writer.shutdown().await?;
            Ok(true)
        }
        Ok(Err(error)) => Err(error),
        Err(_) => Ok(false),
    }
}

/// Writes one ordinary frame, adopting the drain deadline the moment the
/// reserved notice appears.
async fn write_ordinary(
    writer: &mut OwnedWriteHalf,
    bytes: &[u8],
    notice: &mut Option<ReservedNotice>,
    reserve_open: &mut bool,
    reserved: &mut mpsc::Receiver<ReservedNotice>,
) -> std::io::Result<()> {
    let write = writer.write_all(bytes);
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

/// Dispatches one decoded frame. Returns `Ok(true)` when the connection must
/// close (fatal protocol error or rejected handshake).
fn handle_frame(
    frame: WireFrame,
    context: &ConnectionContext,
    drain: &watch::Receiver<Option<DrainNotice>>,
    lane: &OutboundLane,
    grant: &mut Option<ConnectionGrant>,
    outbound_limit: &mut usize,
) -> Result<bool, DaemonError> {
    let Some(granted) = grant.as_ref() else {
        let WireFrame::Hello(hello) = frame else {
            enqueue_fatal(
                lane,
                "handshake_required",
                "Hello must be the first frame",
                *outbound_limit,
            )?;
            return Ok(true);
        };
        // From here on the client's max_receive_frame caps everything we
        // send, including the Welcome itself and any rejection.
        *outbound_limit = context.frame_limit.min(hello.max_receive_frame as usize);
        return negotiate_hello(hello, context, drain, lane, grant, *outbound_limit);
    };

    match frame {
        WireFrame::Ping { nonce } => {
            enqueue(lane, &WireFrame::Pong { nonce }, *outbound_limit)?;
            Ok(false)
        }
        // W3b2 seam: the session hub will route Request bodies; until then
        // every request gets the typed draining/not_found stub.
        WireFrame::Request { request_id, .. } => {
            enqueue_stub(request_id, drain, lane, *outbound_limit)?;
            Ok(false)
        }
        // Authorization is already real: `MenuAnswer` is the one control frame
        // this lane accepts, so a connection granted only `view` is refused
        // here rather than in W3b2. Arbitration (durable compare-and-set
        // answers) is what replaces the stub below.
        WireFrame::MenuAnswer { request_id, .. } => {
            let (code, message) = if !granted.grants(Capability::Control) {
                (
                    ERROR_CODE_CAPABILITY_DENIED,
                    "this connection was not granted the control capability",
                )
            } else if drain.borrow().is_some() {
                (ERROR_CODE_DRAINING, "daemon is draining")
            } else {
                (
                    ERROR_CODE_NOT_FOUND,
                    "menu routing is not available until W3b2",
                )
            };
            // A client that correlated its answer gets a correlated reply; one
            // that did not still gets the uncorrelated connection-level form.
            let frame = match request_id {
                Some(request_id) => WireFrame::Response {
                    request_id,
                    body: ResponseBody::Error {
                        code: code.into(),
                        message: message.into(),
                        retryable: code == ERROR_CODE_DRAINING,
                        data: Option::<ErrorData>::None,
                    },
                },
                None => WireFrame::ProtocolError(ProtocolError {
                    code: code.into(),
                    message: message.into(),
                    fatal: false,
                }),
            };
            enqueue(lane, &frame, *outbound_limit)?;
            Ok(false)
        }
        WireFrame::Unknown => {
            enqueue(
                lane,
                &WireFrame::ProtocolError(ProtocolError {
                    code: "unknown_frame".into(),
                    message: "frame kind is not implemented".into(),
                    fatal: false,
                }),
                *outbound_limit,
            )?;
            Ok(false)
        }
        _ => {
            enqueue_fatal(
                lane,
                "unexpected_frame",
                "frame is not valid from a connected client",
                *outbound_limit,
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
) -> Result<bool, DaemonError> {
    let server_range = ServerRange {
        protocol_min: haider_rpc::WIRE_PROTOCOL_VERSION,
        protocol_max: haider_rpc::WIRE_PROTOCOL_VERSION,
        capabilities: CapabilitySet::from([Capability::View, Capability::Control]),
    };
    let negotiated = match negotiate(&hello, &server_range) {
        Ok(negotiated) => negotiated,
        Err(error) => {
            enqueue(lane, &WireFrame::ProtocolError(error), outbound_limit)?;
            return Ok(true);
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
    enqueue(
        lane,
        &WireFrame::Welcome(Welcome {
            protocol: negotiated.protocol,
            instance_id: context.instance_id.clone(),
            daemon_generation: context.daemon_generation,
            frame_limit,
            profile_id: context.profile_id.clone(),
            daemon_version: env!("CARGO_PKG_VERSION").into(),
            lifecycle_phase,
            capabilities_granted: negotiated.capabilities_granted.clone(),
        }),
        outbound_limit,
    )?;
    // Retained, not discarded: the grant is what later frames are authorized
    // against (W3b2 reads it through `ConnectionGrant`).
    *grant = Some(ConnectionGrant {
        capabilities: negotiated.capabilities_granted,
    });
    Ok(false)
}

/// W3b2 seam: typed `Response::Error` stub for every session RPC.
/// `draining` (retryable — try the next generation) once shutdown started,
/// `not_found` (not retryable) otherwise.
fn enqueue_stub(
    request_id: RequestId,
    drain: &watch::Receiver<Option<DrainNotice>>,
    lane: &OutboundLane,
    outbound_limit: usize,
) -> Result<(), DaemonError> {
    let (code, message, retryable) = if drain.borrow().is_some() {
        (ERROR_CODE_DRAINING, "daemon is draining", true)
    } else {
        (
            ERROR_CODE_NOT_FOUND,
            "session RPC is not available until W3b2",
            false,
        )
    };
    enqueue(
        lane,
        &WireFrame::Response {
            request_id,
            body: ResponseBody::Error {
                code: code.into(),
                message: message.into(),
                retryable,
                data: Option::<ErrorData>::None,
            },
        },
        outbound_limit,
    )
}

fn enqueue_fatal(
    lane: &OutboundLane,
    code: &str,
    message: &str,
    outbound_limit: usize,
) -> Result<(), DaemonError> {
    enqueue(
        lane,
        &WireFrame::ProtocolError(ProtocolError {
            code: code.into(),
            message: message.into(),
            fatal: true,
        }),
        outbound_limit,
    )
}

/// Non-blocking enqueue (R12): a full queue — in frames OR in queued bytes —
/// means the client is not reading its own replies, and the connection errors
/// out instead of stalling the daemon.
fn enqueue(
    lane: &OutboundLane,
    frame: &WireFrame,
    outbound_limit: usize,
) -> Result<(), DaemonError> {
    let bytes = encode_outbound(frame, outbound_limit)?;
    let charged = bytes.len();
    lane.queued_bytes
        .charge(charged)
        .map_err(|queued| DaemonError::Protocol {
            message: format!(
                "connection queued-byte budget exhausted: {charged} more bytes with {queued} of {} queued",
                lane.queued_bytes.budget
            ),
        })?;
    lane.frames.try_send(bytes).map_err(|error| {
        // A refused frame must not leave its charge behind.
        lane.queued_bytes.credit(charged);
        DaemonError::Protocol {
            message: format!("bounded connection queue unavailable: {error}"),
        }
    })
}

fn encode_outbound(frame: &WireFrame, outbound_limit: usize) -> Result<Vec<u8>, DaemonError> {
    uds_codec::encode(frame, outbound_limit).map_err(|error| DaemonError::Protocol {
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
) -> Result<Vec<u8>, DaemonError> {
    let mut reason = notice.reason.clone();
    loop {
        let frame = WireFrame::ServerDraining {
            reason: reason.clone(),
            instance_id: notice.instance_id.clone(),
            daemon_generation: notice.daemon_generation,
            deadline_unix_ms: notice.deadline_unix_ms,
        };
        match uds_codec::encode(&frame, outbound_limit) {
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
pub(crate) fn reject_over_limit(stream: &UnixStream, context: &ConnectionContext) {
    if !stream
        .peer_cred()
        .is_ok_and(|credentials| credentials.uid() == context.owner_uid)
    {
        return;
    }
    let frame = WireFrame::ProtocolError(ProtocolError {
        code: ERROR_CODE_OVERLOADED.into(),
        message: format!(
            "daemon is serving its maximum of {} connections; retry later",
            context.max_connections
        ),
        fatal: true,
    });
    let Ok(bytes) = encode_outbound(&frame, context.frame_limit) else {
        return;
    };
    let mut written = 0;
    while written < bytes.len() {
        match rustix::io::write(stream, &bytes[written..]) {
            Ok(0) => break,
            Ok(count) => written = written.saturating_add(count),
            Err(rustix::io::Errno::INTR) => {}
            Err(_) => break,
        }
    }
}
