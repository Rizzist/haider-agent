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
//! - one `ServerDraining` is the last frame of a draining connection (R17),
//!   and it travels a reserved path so ordinary replies can never spend the
//!   queue slot or the bytes that last frame needs;
//! - a peer accepted beyond the daemon's connection cap is answered with a
//!   fatal `overloaded` error and closed without ever entering this layer's
//!   task/queue accounting (report §2.5).
//!
//! W3b2 seams: `Request` bodies and `MenuAnswer` are answered with typed
//! `draining` / `not_found` stubs (see [`handle_frame`] / [`enqueue_stub`]);
//! session hub, attach/replay, and menu arbitration replace them in W3b2.

use crate::DaemonError;
use haider_rpc::{
    Capability, CapabilitySet, ERROR_CODE_DRAINING, ERROR_CODE_NOT_FOUND, ERROR_CODE_OVERLOADED,
    ErrorData, Hello, LifecyclePhase, ProtocolError, RequestId, ResponseBody, ServerRange, Welcome,
    WireFrame, negotiate, uds_codec,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot, watch};

/// Owns the socket write half; aborted on drop so a peer that never reads
/// cannot leak a writer task past its connection.
struct WriterTask(Option<tokio::task::JoinHandle<std::io::Result<()>>>);

impl WriterTask {
    async fn finish(mut self) -> Result<std::io::Result<()>, tokio::task::JoinError> {
        match self.0.take() {
            Some(task) => task.await,
            None => Ok(Ok(())),
        }
    }
}

impl Drop for WriterTask {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
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
    pub(crate) deadline_unix_ms: u64,
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
    /// UID that owns the endpoint; every peer must match it (R2).
    pub(crate) owner_uid: u32,
    /// For error context only; the stream is already accepted.
    pub(crate) endpoint_path: PathBuf,
}

/// Runs one client connection to completion: UID gate, framed read loop,
/// bounded write queue, and a final `ServerDraining` + close when the drain
/// broadcast fires. Errors returned here end only this connection — the
/// accept loop in `runtime.rs` deliberately ignores them.
pub(crate) async fn serve(
    stream: UnixStream,
    context: ConnectionContext,
    mut drain: watch::Receiver<Option<DrainNotice>>,
) -> Result<(), DaemonError> {
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

    let (mut reader, mut writer) = stream.into_split();
    let (outbound, mut queued) = mpsc::channel::<Vec<u8>>(context.outbound_queue_capacity);
    let lane = OutboundLane {
        frames: outbound,
        queued_bytes: Arc::new(QueuedBytes::new(context.outbound_queued_bytes)),
    };
    // Reserved drain path (R17): one dedicated slot, outside the ordinary
    // queue and outside its byte budget, so no volume of ordinary replies can
    // consume what the last frame needs.
    let (drain_frame, drain_frame_reserved) = oneshot::channel::<Vec<u8>>();
    let mut drain_frame = Some(drain_frame);
    let written_bytes = Arc::clone(&lane.queued_bytes);
    let writer_task = WriterTask(Some(tokio::spawn(async move {
        while let Some(bytes) = queued.recv().await {
            let charged = bytes.len();
            let result = writer.write_all(&bytes).await;
            written_bytes.credit(charged);
            result?;
        }
        // The reserved frame is written after every ordinary frame, so
        // `ServerDraining` stays the last frame of a draining connection.
        if let Ok(notice) = drain_frame_reserved.await {
            writer.write_all(&notice).await?;
        }
        writer.shutdown().await
    })));

    let mut decoder = uds_codec::Decoder::new(context.frame_limit);
    let mut buffer = [0_u8; 16 * 1024];
    let mut handshaken = false;
    let mut outbound_limit = context.frame_limit;
    let mut close = false;

    while !close {
        tokio::select! {
            changed = drain.changed() => {
                let notice = changed.is_ok().then(|| drain.borrow().clone()).flatten();
                if let Some(notice) = notice {
                    let frame = WireFrame::ServerDraining {
                        reason: notice.reason,
                        instance_id: notice.instance_id,
                        daemon_generation: notice.daemon_generation,
                        deadline_unix_ms: notice.deadline_unix_ms,
                    };
                    let bytes = encode_outbound(&frame, outbound_limit)?;
                    if let Some(reserved) = drain_frame.take() {
                        // Never blocks and never charged: the reserve exists
                        // precisely so queue pressure cannot lose this frame.
                        let _ = reserved.send(bytes);
                    }
                }
                break;
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
                        &mut handshaken,
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

    // Closing the ordinary lane lets the writer flush what is already queued;
    // dropping the reserve releases its wait when no drain notice was sent.
    drop(lane);
    drop(drain_frame);
    writer_task
        .finish()
        .await
        .map_err(|error| DaemonError::Task {
            message: format!("connection writer task failed: {error}"),
        })?
        .map_err(|error| DaemonError::io("write Unix connection", &context.endpoint_path, error))
}

/// Dispatches one decoded frame. Returns `Ok(true)` when the connection must
/// close (fatal protocol error or rejected handshake).
fn handle_frame(
    frame: WireFrame,
    context: &ConnectionContext,
    drain: &watch::Receiver<Option<DrainNotice>>,
    lane: &OutboundLane,
    handshaken: &mut bool,
    outbound_limit: &mut usize,
) -> Result<bool, DaemonError> {
    if !*handshaken {
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
        return negotiate_hello(hello, context, drain, lane, handshaken, *outbound_limit);
    }

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
        // W3b2 seam: menu arbitration (durable compare-and-set answers)
        // replaces this stub.
        WireFrame::MenuAnswer { .. } => {
            let (code, message) = if drain.borrow().is_some() {
                (ERROR_CODE_DRAINING, "daemon is draining")
            } else {
                (
                    ERROR_CODE_NOT_FOUND,
                    "menu routing is not available until W3b2",
                )
            };
            enqueue(
                lane,
                &WireFrame::ProtocolError(ProtocolError {
                    code: code.into(),
                    message: message.into(),
                    fatal: false,
                }),
                *outbound_limit,
            )?;
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
    handshaken: &mut bool,
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
            capabilities_granted: negotiated.capabilities_granted,
        }),
        outbound_limit,
    )?;
    *handshaken = true;
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
