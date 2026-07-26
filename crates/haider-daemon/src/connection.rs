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
//! - the outbound queue is bounded and never blocks the daemon on a slow
//!   client (R12's mechanism): a full queue is a connection error, and the
//!   store — not the socket — is the lag buffer in later lanes;
//! - one `ServerDraining` is the last frame of a draining connection (R17).
//!
//! W3b2 seams: `Request` bodies and `MenuAnswer` are answered with typed
//! `draining` / `not_found` stubs (see [`handle_frame`] / [`enqueue_stub`]);
//! session hub, attach/replay, and menu arbitration replace them in W3b2.

use crate::DaemonError;
use haider_rpc::{
    Capability, CapabilitySet, ERROR_CODE_DRAINING, ERROR_CODE_NOT_FOUND, ErrorData, Hello,
    LifecyclePhase, ProtocolError, RequestId, ResponseBody, ServerRange, Welcome, WireFrame,
    negotiate, uds_codec,
};
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, watch};

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
    let writer_task = WriterTask(Some(tokio::spawn(async move {
        while let Some(bytes) = queued.recv().await {
            writer.write_all(&bytes).await?;
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
                    enqueue_wait(&outbound, &frame, outbound_limit).await?;
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
                        &outbound,
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
                        &outbound,
                        &WireFrame::ProtocolError(protocol_error),
                        outbound_limit,
                    );
                    close = true;
                }
            }
        }
    }

    drop(outbound);
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
    outbound: &mpsc::Sender<Vec<u8>>,
    handshaken: &mut bool,
    outbound_limit: &mut usize,
) -> Result<bool, DaemonError> {
    if !*handshaken {
        let WireFrame::Hello(hello) = frame else {
            enqueue_fatal(
                outbound,
                "handshake_required",
                "Hello must be the first frame",
                *outbound_limit,
            )?;
            return Ok(true);
        };
        // From here on the client's max_receive_frame caps everything we
        // send, including the Welcome itself and any rejection.
        *outbound_limit = context.frame_limit.min(hello.max_receive_frame as usize);
        return negotiate_hello(hello, context, drain, outbound, handshaken, *outbound_limit);
    }

    match frame {
        WireFrame::Ping { nonce } => {
            enqueue(outbound, &WireFrame::Pong { nonce }, *outbound_limit)?;
            Ok(false)
        }
        // W3b2 seam: the session hub will route Request bodies; until then
        // every request gets the typed draining/not_found stub.
        WireFrame::Request { request_id, .. } => {
            enqueue_stub(request_id, drain, outbound, *outbound_limit)?;
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
                outbound,
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
                outbound,
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
                outbound,
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
    outbound: &mpsc::Sender<Vec<u8>>,
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
            enqueue(outbound, &WireFrame::ProtocolError(error), outbound_limit)?;
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
        outbound,
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
    outbound: &mpsc::Sender<Vec<u8>>,
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
        outbound,
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
    outbound: &mpsc::Sender<Vec<u8>>,
    code: &str,
    message: &str,
    outbound_limit: usize,
) -> Result<(), DaemonError> {
    enqueue(
        outbound,
        &WireFrame::ProtocolError(ProtocolError {
            code: code.into(),
            message: message.into(),
            fatal: true,
        }),
        outbound_limit,
    )
}

/// Non-blocking enqueue (R12): a full queue means the client is not reading
/// its own replies, and the connection errors out instead of stalling the
/// daemon.
fn enqueue(
    outbound: &mpsc::Sender<Vec<u8>>,
    frame: &WireFrame,
    outbound_limit: usize,
) -> Result<(), DaemonError> {
    let bytes =
        uds_codec::encode(frame, outbound_limit).map_err(|error| DaemonError::Protocol {
            message: format!("outbound frame rejected by peer limit: {error}"),
        })?;
    outbound
        .try_send(bytes)
        .map_err(|error| DaemonError::Protocol {
            message: format!("bounded connection queue unavailable: {error}"),
        })
}

/// Awaiting enqueue, used only for the final `ServerDraining` frame: the
/// drain notice must not be dropped for queue pressure, and waiting is safe
/// because it is the last frame before this connection closes.
async fn enqueue_wait(
    outbound: &mpsc::Sender<Vec<u8>>,
    frame: &WireFrame,
    outbound_limit: usize,
) -> Result<(), DaemonError> {
    let bytes =
        uds_codec::encode(frame, outbound_limit).map_err(|error| DaemonError::Protocol {
            message: format!("outbound frame rejected by peer limit: {error}"),
        })?;
    outbound
        .send(bytes)
        .await
        .map_err(|error| DaemonError::Protocol {
            message: format!("bounded connection queue closed during drain: {error}"),
        })
}
