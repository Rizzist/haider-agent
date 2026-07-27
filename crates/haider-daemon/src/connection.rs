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
#[path = "connection_tests.rs"]
mod connection_tests;

use crate::DaemonError;
use crate::session_hub::{FrameSendError, FrameSink, HubConnection, SessionHub};
use haider_rpc::{
    AttachmentId, Capability, CapabilitySet, ERROR_CODE_OVERLOADED, Hello, LifecyclePhase,
    ProtocolError, ServerRange, Welcome, WireFrame, negotiate, uds_codec,
};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::{Notify, mpsc, oneshot, watch};
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

/// One fair scheduling lane. System replies share one lane; each attachment
/// owns another. The writer visits active lanes round-robin.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum LaneKey {
    System,
    Attachment(AttachmentId),
}

#[derive(Debug)]
struct QueuedFrame {
    bytes: Vec<u8>,
}

#[derive(Debug)]
struct OutboundState {
    lanes: HashMap<LaneKey, VecDeque<QueuedFrame>>,
    round_robin: VecDeque<LaneKey>,
    queued_frames: usize,
    /// Includes the frame currently being written; credited only when that
    /// write settles, preserving W3b1's real allocation bound.
    queued_bytes: usize,
    closed: bool,
}

/// Explicit-close, bounded, fair per-connection outbox.
///
/// Clones carry enqueue authority but cannot keep the writer alive after the
/// connection owner calls [`Self::close`]. One hot attachment is capped at
/// half the global frame bound, leaving admission room for another lane.
#[derive(Clone)]
struct OutboundLane {
    inner: Arc<OutboundQueue>,
}

struct OutboundQueue {
    state: Mutex<OutboundState>,
    ready: Notify,
    frame_capacity: usize,
    byte_budget: usize,
    per_lane_capacity: usize,
}

impl OutboundLane {
    fn new(frame_capacity: usize, byte_budget: usize) -> Self {
        Self {
            inner: Arc::new(OutboundQueue {
                state: Mutex::new(OutboundState {
                    lanes: HashMap::new(),
                    round_robin: VecDeque::new(),
                    queued_frames: 0,
                    queued_bytes: 0,
                    closed: false,
                }),
                ready: Notify::new(),
                frame_capacity,
                byte_budget,
                per_lane_capacity: frame_capacity.saturating_add(1).saturating_div(2).max(1),
            }),
        }
    }

    fn try_push(&self, key: LaneKey, bytes: Vec<u8>) -> Result<(), DaemonError> {
        let charged = bytes.len();
        let mut state = self.inner.state.lock().map_err(|_| DaemonError::Task {
            message: "connection outbox mutex is poisoned".into(),
        })?;
        let lane_len = state.lanes.get(&key).map_or(0, VecDeque::len);
        let next_bytes = state.queued_bytes.checked_add(charged);
        if state.closed
            || state.queued_frames >= self.inner.frame_capacity
            || lane_len >= self.inner.per_lane_capacity
            || next_bytes.is_none_or(|total| total > self.inner.byte_budget)
        {
            return Err(DaemonError::Protocol {
                message: format!(
                    "bounded fair connection outbox unavailable: {charged} more bytes with {} frames and {} bytes queued",
                    state.queued_frames, state.queued_bytes
                ),
            });
        }
        let activate = lane_len == 0;
        state
            .lanes
            .entry(key.clone())
            .or_default()
            .push_back(QueuedFrame { bytes });
        if activate {
            state.round_robin.push_back(key);
        }
        state.queued_frames = state.queued_frames.saturating_add(1);
        state.queued_bytes = next_bytes.unwrap_or(state.queued_bytes);
        drop(state);
        self.inner.ready.notify_one();
        Ok(())
    }

    async fn recv(&self) -> Option<QueuedFrame> {
        loop {
            let notified = self.inner.ready.notified();
            {
                let mut state = match self.inner.state.lock() {
                    Ok(state) => state,
                    Err(_) => return None,
                };
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

    fn credit(&self, bytes: usize) {
        if let Ok(mut state) = self.inner.state.lock() {
            state.queued_bytes = state.queued_bytes.saturating_sub(bytes);
        }
    }

    fn purge(&self, attachment_id: &AttachmentId) {
        let Ok(mut state) = self.inner.state.lock() else {
            return;
        };
        let key = LaneKey::Attachment(attachment_id.clone());
        if let Some(frames) = state.lanes.remove(&key) {
            let count = frames.len();
            let bytes = frames.iter().fold(0_usize, |total, frame| {
                total.saturating_add(frame.bytes.len())
            });
            state.queued_frames = state.queued_frames.saturating_sub(count);
            state.queued_bytes = state.queued_bytes.saturating_sub(bytes);
        }
        state.round_robin.retain(|candidate| candidate != &key);
    }

    fn close(&self) {
        if let Ok(mut state) = self.inner.state.lock() {
            state.closed = true;
        }
        self.inner.ready.notify_waiters();
    }
}

struct ConnectionFrameSink {
    lane: OutboundLane,
    outbound_limit: usize,
}

impl FrameSink for ConnectionFrameSink {
    fn try_send(&self, frame: WireFrame) -> Result<(), FrameSendError> {
        let key = attachment_lane(&frame);
        let bytes = encode_outbound(&frame, self.outbound_limit).map_err(|_| FrameSendError)?;
        self.lane.try_push(key, bytes).map_err(|_| FrameSendError)
    }

    fn purge_attachment(&self, attachment_id: &AttachmentId) {
        self.lane.purge(attachment_id);
    }
}

fn attachment_lane(frame: &WireFrame) -> LaneKey {
    match frame {
        WireFrame::Event { attachment_id, .. }
        | WireFrame::AttachCaughtUp { attachment_id, .. }
        | WireFrame::Lagged { attachment_id, .. } => LaneKey::Attachment(attachment_id.clone()),
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
    let lane = OutboundLane::new(
        context.outbound_queue_capacity,
        context.outbound_queued_bytes,
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

    let mut decoder = uds_codec::Decoder::new(context.frame_limit);
    let mut buffer = [0_u8; 16 * 1024];
    // Inbound capacity is deliberately one handler: bytes enter through this
    // fixed read buffer and bounded decoder, and every decoded command is
    // completed serially in this connection task. No detached handler set or
    // unbounded inbound command queue exists; cancelling/joining this owned
    // connection task cancels its sole in-flight handler.
    let mut grant = Option::<ConnectionGrant>::None;
    let mut hub_connection = Option::<HubConnection>::None;
    let mut outbound_limit = context.frame_limit;
    let mut close = false;
    let mut notice_reserved = false;
    let mut notice_refused = false;
    // A peer that never speaks must not hold its connection slot forever.
    let handshake_deadline = tokio::time::sleep(context.handshake_timeout);
    tokio::pin!(handshake_deadline);

    let processing = async {
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
                            &mut hub_connection,
                            &mut outbound_limit,
                        ).await?;
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
        .map_err(|error| DaemonError::io("write Unix connection", &context.endpoint_path, error))?;
    processing?;
    Ok(match (notice_reserved || notice_refused, written) {
        (false, _) => ConnectionExit::ClosedBeforeDrain,
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
async fn run_writer(
    mut writer: OwnedWriteHalf,
    queued: OutboundLane,
    mut reserved: mpsc::Receiver<ReservedNotice>,
) -> std::io::Result<bool> {
    let mut notice = Option::<ReservedNotice>::None;
    let mut reserve_open = true;
    let mut notice_written = false;
    let mut drain_deadline = Option::<Instant>::None;
    loop {
        let next = if reserve_open && !notice_written {
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
            let charged = frame.bytes.len();
            let result = if notice_written {
                match drain_deadline {
                    Some(deadline) => {
                        match tokio::time::timeout_at(deadline, writer.write_all(&frame.bytes))
                            .await
                        {
                            Ok(result) => result,
                            Err(_) => Err(std::io::Error::new(
                                std::io::ErrorKind::TimedOut,
                                "drain deadline expired while writing checkpoint traffic",
                            )),
                        }
                    }
                    None => writer.write_all(&frame.bytes).await,
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
            // its bytes, which is what makes the budget a real memory bound.
            queued.credit(charged);
            match result {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
                    return Ok(notice_written);
                }
                Err(error) => return Err(error),
            }
        } else if notice.is_none() {
            break;
        }
        if !notice_written && let Some(reserved_notice) = notice.take() {
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
async fn handle_frame(
    frame: WireFrame,
    context: &ConnectionContext,
    drain: &watch::Receiver<Option<DrainNotice>>,
    lane: &OutboundLane,
    grant: &mut Option<ConnectionGrant>,
    hub_connection: &mut Option<HubConnection>,
    outbound_limit: &mut usize,
) -> Result<bool, DaemonError> {
    let Some(_) = grant.as_ref() else {
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
        let close = negotiate_hello(hello, context, drain, lane, grant, *outbound_limit)?;
        if !close && let Some(granted) = grant.as_ref() {
            let sink: Arc<dyn FrameSink> = Arc::new(ConnectionFrameSink {
                lane: lane.clone(),
                outbound_limit: *outbound_limit,
            });
            *hub_connection = Some(
                context
                    .hub
                    .open_connection(granted.capabilities.clone(), sink)
                    .map_err(DaemonError::from)?,
            );
        }
        return Ok(close);
    };

    match frame {
        WireFrame::Ping { nonce } => {
            enqueue(lane, &WireFrame::Pong { nonce }, *outbound_limit)?;
            Ok(false)
        }
        WireFrame::Request { request_id, body } => {
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
    lane.try_push(LaneKey::System, bytes)
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
