//! Reusable UDS RPC client (report §6.2).
//!
//! One [`RpcClient`] owns one negotiated connection: a bounded writer task, a
//! reader task that correlates `Response` frames to pending requests by
//! `RequestId`, an R9 heartbeat task (`Ping` after each 15 s cadence; a ping unmatched for
//! 45 s declares the connection dead), and a bounded event channel for
//! everything that is not a correlated response. Reconnection is a caller
//! primitive: the client never reconnects silently — it reports a typed
//! [`DisconnectReason`] and the caller dials again with [`connect`].
//!
//! Secret hygiene: every outbound encoded frame buffer is zeroized after its
//! socket write (`Zeroizing`), so a staged secret's bytes do not linger in a
//! freed queue buffer (R7's sensitive-path rule; the daemon side zeroizes
//! inbound bodies).

use std::collections::{HashMap, VecDeque};
use std::io::IoSlice;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use haider_platform::{IpcReadHalf, IpcStream, IpcWriteHalf};
use haider_rpc::haider_protocol::ids::{MenuId, SessionId};
use haider_rpc::{
    Capability, CapabilitySet, ClientKind, CodecError, CommandId, DEFAULT_FRAME_LIMIT, Hello,
    MenuInput, ProtocolError, RequestBody, RequestId, ResponseBody, WIRE_PROTOCOL_VERSION, Welcome,
    WireEncoding, WireFrame, uds_codec,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;
use zeroize::Zeroizing;

/// R9 client heartbeat cadence.
pub const PING_INTERVAL: Duration = Duration::from_secs(15);
/// R9 client liveness deadline: a ping unmatched this long declares the
/// connection dead and the caller reconnects.
pub const PONG_DEADLINE: Duration = Duration::from_secs(45);
/// Default handshake deadline (mirrors the daemon's own handshake timeout).
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Default continuous deadline from correlated-request admission through its
/// response. Long-running work is accepted by a quick RPC and then observed as
/// events; no ordinary request is allowed to wait forever while heartbeats pass.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

const OUTBOUND_CAPACITY: usize = 64;
const EVENT_CAPACITY: usize = 256;

/// Durable coordinates for the existing receipted top-level menu-answer
/// frame. Callers must hold a Control attachment to `session_id`.
#[derive(Debug, Clone)]
pub struct MenuAnswerRequest {
    pub command_id: CommandId,
    pub session_id: SessionId,
    pub menu_id: MenuId,
    pub request_seq: u64,
    pub worker_generation: u64,
    pub option_key: String,
    pub option_index: u32,
    pub input: Option<MenuInput>,
}

#[derive(Clone, Copy)]
struct NegotiatedCodec {
    frame_limit: usize,
    encoding: WireEncoding,
}

/// Connection parameters for [`connect`].
#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub client_name: String,
    pub client_version: String,
    pub client_instance_id: String,
    pub client_kind: ClientKind,
    pub capabilities: CapabilitySet,
    /// Largest JSON body this client will accept (advertised in `Hello`).
    pub frame_limit: usize,
    pub handshake_timeout: Duration,
    pub request_timeout: Duration,
    pub ping_interval: Duration,
    pub pong_deadline: Duration,
    /// Whether this connection will consume asynchronous events and remain
    /// alive long enough to need heartbeat liveness.
    pub connection_usage: ConnectionUsage,
    /// Content-free timing context for an explicitly traced one-shot client.
    #[doc(hidden)]
    pub turn_trace: Option<Arc<crate::client_trace::ClientEnvelopeTrace>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ConnectionUsage {
    #[default]
    LongLived,
    OneShot,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            client_name: "haider".into(),
            client_version: env!("CARGO_PKG_VERSION").into(),
            client_instance_id: String::new(),
            client_kind: ClientKind::Cli,
            capabilities: CapabilitySet::from([Capability::View, Capability::Control]),
            frame_limit: DEFAULT_FRAME_LIMIT,
            handshake_timeout: HANDSHAKE_TIMEOUT,
            request_timeout: REQUEST_TIMEOUT,
            ping_interval: PING_INTERVAL,
            pong_deadline: PONG_DEADLINE,
            connection_usage: ConnectionUsage::LongLived,
            turn_trace: None,
        }
    }
}

/// Why [`connect`] could not produce a negotiated connection.
#[derive(Debug)]
pub enum ConnectError {
    /// No socket node exists — the auto-spawn trigger class.
    NotFound(std::io::Error),
    /// A socket node exists but nothing serves it — the other auto-spawn
    /// trigger class (stale-socket recovery belongs to the daemon, never
    /// the client).
    Refused(std::io::Error),
    /// The endpoint exists but this process may not use it. Never a spawn
    /// trigger.
    PermissionDenied(std::io::Error),
    /// Any other transport failure.
    Io(std::io::Error),
    /// The daemon rejected the handshake with a protocol error
    /// (`protocol_version_mismatch` is the no-overlap case).
    Rejected(ProtocolError),
    /// The peer closed before completing the handshake.
    ClosedDuringHandshake,
    /// The handshake did not finish before the deadline.
    HandshakeTimeout,
    /// The peer sent bytes that do not decode as wire frames.
    Frame(CodecError),
    /// The peer answered `Hello` with something other than
    /// `Welcome`/`ProtocolError` — a malformed handshake, never a spawn
    /// trigger.
    UnexpectedFrame,
}

impl ConnectError {
    /// Whether bare `haider` may respond to this failure by spawning a
    /// daemon (R8 step 5: ONLY NotFound / ConnectionRefused).
    pub fn is_spawnable(&self) -> bool {
        matches!(self, Self::NotFound(_) | Self::Refused(_))
    }
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(error) => write!(formatter, "daemon endpoint does not exist: {error}"),
            Self::Refused(error) => write!(formatter, "daemon endpoint refused: {error}"),
            Self::PermissionDenied(error) => {
                write!(formatter, "daemon endpoint permission denied: {error}")
            }
            Self::Io(error) => write!(formatter, "daemon connection failed: {error}"),
            Self::Rejected(error) => write!(formatter, "daemon rejected handshake: {error}"),
            Self::ClosedDuringHandshake => {
                formatter.write_str("daemon closed the connection during the handshake")
            }
            Self::HandshakeTimeout => formatter.write_str("daemon handshake timed out"),
            Self::Frame(error) => write!(formatter, "daemon handshake framing failed: {error}"),
            Self::UnexpectedFrame => {
                formatter.write_str("daemon answered the handshake with an unexpected frame")
            }
        }
    }
}

impl std::error::Error for ConnectError {}

/// Why an established connection stopped serving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisconnectReason {
    /// [`RpcClient::close`] was called locally.
    Closed,
    /// The daemon closed the socket.
    PeerClosed,
    /// A transport read/write failed.
    Io(String),
    /// The peer sent undecodable bytes.
    Protocol(String),
    /// The daemon sent a fatal `ProtocolError` frame.
    Fatal(ProtocolError),
    /// R9: no matching `Pong` within the pong deadline — reconnect.
    PongTimeout,
    /// One correlated request made no response progress before its continuous
    /// request deadline. The connection is retired so a late response cannot
    /// be mistaken for a later attempt.
    RequestTimeout {
        request_id: String,
        timeout: Duration,
    },
}

impl std::fmt::Display for DisconnectReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => formatter.write_str("connection closed locally"),
            Self::PeerClosed => formatter.write_str("daemon closed the connection"),
            Self::Io(message) => write!(formatter, "connection transport failed: {message}"),
            Self::Protocol(message) => write!(formatter, "connection framing failed: {message}"),
            Self::Fatal(error) => write!(formatter, "daemon reported a fatal error: {error}"),
            Self::PongTimeout => {
                formatter.write_str("daemon made no ping progress within the pong deadline")
            }
            Self::RequestTimeout {
                request_id,
                timeout,
            } => write!(
                formatter,
                "daemon did not answer {request_id} within {} ms",
                timeout.as_millis()
            ),
        }
    }
}

/// What [`RpcClient::close`] proved at the local transport boundary.
#[must_use = "inspect whether the peer was notified or explicitly discard the close outcome"]
#[derive(Debug)]
pub enum ClientCloseOutcome {
    /// The peer was synchronously notified of the close. Unix provides this
    /// through `shutdown(2)` without another executor poll.
    PeerNotified,
    /// The local stream slot was emptied, but the owning Tokio runtime must be
    /// polled before the close becomes peer-visible. This is the Windows
    /// named-pipe outcome while cancelled IOCP operations are still pending.
    LocalSlotEmptiedOnly,
    /// The peer had already gone away, or a prior close already consumed the
    /// transport shutdown authority.
    AlreadyClosed,
    /// The platform shutdown request itself failed.
    ShutdownFailed(std::io::Error),
}

/// A correlated request failed.
#[derive(Debug)]
pub enum ClientError {
    /// The connection is (or became) dead; the reason is the reconnect cue.
    Disconnected(DisconnectReason),
    /// The request body could not be encoded within the negotiated limit.
    Encode(CodecError),
    /// A request used an additive shape the peer did not advertise. Nothing
    /// was encoded or sent.
    MissingFeature(&'static str),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disconnected(reason) => write!(formatter, "request failed: {reason}"),
            Self::Encode(error) => write!(formatter, "request could not be encoded: {error}"),
            Self::MissingFeature(feature) => write!(
                formatter,
                "missing_feature: daemon does not advertise required feature `{feature}`"
            ),
        }
    }
}

impl std::error::Error for ClientError {}

/// A successfully negotiated connection.
pub struct Connected {
    pub client: RpcClient,
    pub welcome: Welcome,
    /// Kernel-authenticated credentials for the process serving this UDS.
    ///
    /// Update uses these credentials while retaining this connection: the
    /// PID comes from the socket peer, never the profile owner diagnostics.
    /// Platforms supported by Haider expose a PID; keeping it optional
    /// makes an unavailable credential a typed update refusal rather than a
    /// regression for ordinary RPC clients.
    pub peer_credentials: PeerCredentials,
}

/// Kernel credentials captured from the connected Unix-domain peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerCredentials {
    pub pid: Option<u32>,
    pub uid: u32,
    pub gid: u32,
}

/// Dials the endpoint and completes the `Hello`/`Welcome` handshake.
pub async fn connect(path: &Path, config: ClientConfig) -> Result<Connected, ConnectError> {
    let mut stream = match haider_platform::connect(path).await {
        Ok(stream) => stream,
        Err(error) => return Err(classify_connect_error(error)),
    };
    if let Some(trace) = config.turn_trace.as_ref() {
        trace.connected();
    }
    // Capture before `RpcClient::start` consumes the stream and splits it.
    // The peer credentials are the update signal authority; the lock-file
    // PID is deliberately never consulted.
    let peer_credentials = haider_platform::peer_credentials(&stream)
        .map(|credentials| PeerCredentials {
            pid: credentials.pid,
            uid: credentials.uid,
            gid: credentials.gid,
        })
        .unwrap_or(PeerCredentials {
            pid: None,
            uid: u32::MAX,
            gid: u32::MAX,
        });
    let hello = WireFrame::Hello(Hello {
        protocol_min: WIRE_PROTOCOL_VERSION,
        protocol_max: WIRE_PROTOCOL_VERSION,
        client_name: config.client_name.clone(),
        client_version: config.client_version.clone(),
        client_instance_id: config.client_instance_id.clone(),
        client_kind: config.client_kind,
        capabilities_requested: config.capabilities.clone(),
        max_receive_frame: u32::try_from(config.frame_limit).unwrap_or(u32::MAX),
        encodings: (std::env::var("HAIDER_WIRE_MSGPACK").as_deref() == Ok("1"))
            .then(|| "msgpack".to_owned())
            .into_iter()
            .collect(),
    });
    let bytes = uds_codec::encode(&hello, config.frame_limit).map_err(ConnectError::Frame)?;
    let handshake = async {
        stream.write_all(&bytes).await.map_err(ConnectError::Io)?;
        read_handshake(&mut stream, config.frame_limit).await
    };
    let (welcome, decoder, leftovers) =
        match tokio::time::timeout(config.handshake_timeout, handshake).await {
            Ok(result) => result?,
            Err(_) => return Err(ConnectError::HandshakeTimeout),
        };
    if let Some(trace) = config.turn_trace.as_ref() {
        trace.hello_done();
    }
    let encoding = match welcome.encoding.as_deref() {
        Some("msgpack") => WireEncoding::MessagePack,
        _ => WireEncoding::Json,
    };
    let client = RpcClient::start(
        stream,
        decoder,
        leftovers,
        &config,
        &welcome,
        encoding,
        peer_credentials,
    )
    .map_err(ConnectError::Io)?;
    Ok(Connected {
        client,
        welcome,
        peer_credentials,
    })
}

/// Reads exactly the always-JSON handshake frame, preserving any bytes from
/// the same read for a fresh decoder using the Welcome's negotiated encoding.
/// The byte boundary is structural: post-handshake bytes are never offered to
/// the JSON decoder, sniffed, or decoded with a fallback codec.
async fn read_handshake<R>(
    reader: &mut R,
    frame_limit: usize,
) -> Result<(Welcome, uds_codec::Decoder, VecDeque<WireFrame>), ConnectError>
where
    R: AsyncRead + Unpin,
{
    let mut decoder = uds_codec::Decoder::new_zeroizing(frame_limit);
    let mut buffer = [0_u8; 16 * 1024];

    loop {
        let read = reader.read(&mut buffer).await.map_err(ConnectError::Io)?;
        if read == 0 {
            return Err(ConnectError::ClosedDuringHandshake);
        }

        let step = decoder.push_one(&buffer[..read]);
        if let Some(error) = step.error {
            return Err(ConnectError::Frame(error));
        }
        let Some(first) = step.frame else {
            continue;
        };
        let welcome = match first {
            WireFrame::Welcome(welcome) => welcome,
            WireFrame::ProtocolError(error) => return Err(ConnectError::Rejected(error)),
            _ => return Err(ConnectError::UnexpectedFrame),
        };
        let encoding = match welcome.encoding.as_deref() {
            Some("msgpack") => WireEncoding::MessagePack,
            _ => WireEncoding::Json,
        };
        // The decoder stopped at the Welcome boundary. Switch the same
        // zeroizing decoder, then decode only the unread coalesced suffix.
        decoder.set_encoding(encoding);
        let remainder = decoder.push(&buffer[step.consumed..read]);
        if let Some(error) = remainder.error {
            return Err(ConnectError::Frame(error));
        }
        return Ok((welcome, decoder, remainder.frames.into()));
    }
}

fn classify_connect_error(error: std::io::Error) -> ConnectError {
    match error.kind() {
        std::io::ErrorKind::NotFound => ConnectError::NotFound(error),
        std::io::ErrorKind::ConnectionRefused => ConnectError::Refused(error),
        std::io::ErrorKind::PermissionDenied => ConnectError::PermissionDenied(error),
        _ => ConnectError::Io(error),
    }
}

/// Connection lifecycle observed by callers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    Connected,
    Disconnected(DisconnectReason),
}

#[derive(Debug, Clone)]
pub(crate) struct ClientHealthSnapshot {
    pub(crate) state: ConnectionState,
    pub(crate) lost_events: u64,
}

pub(crate) enum ClientHealthWait {
    Changed(ClientHealthSnapshot),
    RepairDue {
        timeout: haider_platform::WaitTimeout,
        snapshot: ClientHealthSnapshot,
    },
    Closed(ClientHealthSnapshot),
}

impl ClientHealthWait {
    pub(crate) fn snapshot(&self) -> &ClientHealthSnapshot {
        match self {
            Self::Changed(snapshot) | Self::Closed(snapshot) => snapshot,
            Self::RepairDue { snapshot, .. } => snapshot,
        }
    }

    pub(crate) fn repair_timeout(&self) -> Option<haider_platform::WaitTimeout> {
        match self {
            Self::RepairDue { timeout, .. } => Some(*timeout),
            Self::Changed(_) | Self::Closed(_) => None,
        }
    }

    pub(crate) fn channel_closed(&self) -> bool {
        matches!(self, Self::Closed(_))
    }
}

pub(crate) struct ClientHealthWatch {
    state: watch::Receiver<ConnectionState>,
    lost_events: watch::Receiver<u64>,
    initial: bool,
}

impl ClientHealthWatch {
    fn snapshot(&self) -> ClientHealthSnapshot {
        ClientHealthSnapshot {
            state: self.state.borrow().clone(),
            lost_events: *self.lost_events.borrow(),
        }
    }

    /// Waits for either health signal while retaining a typed periodic repair
    /// probe. Taking `self` keeps this one deadline armed even when the caller
    /// is busy consuming unrelated protocol frames.
    pub(crate) async fn wait(
        mut self,
        operation: &'static str,
        repair_interval: Duration,
    ) -> (Self, ClientHealthWait) {
        if self.initial {
            self.initial = false;
            let snapshot = self.snapshot();
            return (self, ClientHealthWait::Changed(snapshot));
        }
        let outcome = haider_platform::bounded_wait(operation, repair_interval, async {
            tokio::select! {
                changed = self.state.changed() => changed,
                changed = self.lost_events.changed() => changed,
            }
        })
        .await;
        let snapshot = self.snapshot();
        let outcome = match outcome {
            haider_platform::BoundedWait::Completed(Ok(())) => ClientHealthWait::Changed(snapshot),
            haider_platform::BoundedWait::Completed(Err(_)) => ClientHealthWait::Closed(snapshot),
            haider_platform::BoundedWait::TimedOut(timeout) => {
                ClientHealthWait::RepairDue { timeout, snapshot }
            }
        };
        (self, outcome)
    }
}

struct Shared {
    pending: StdMutex<HashMap<String, oneshot::Sender<ResponseBody>>>,
    next_request: AtomicU64,
    /// Highest ping nonce a `Pong` has acknowledged.
    acked_pong: AtomicU64,
    /// Events dropped because the event channel was full. Cursor-bearing
    /// traffic recovers by reattach (R9); an authoritative snapshot without
    /// a cursor instead fails the connection and recovers from its retained
    /// daemon baseline after reconnect.
    lost_events: watch::Sender<u64>,
    state: watch::Sender<ConnectionState>,
    /// A writer death observed while the reader may still be draining
    /// buffered peer frames. The READER promotes this to [`Self::fail`]
    /// when its own end arrives, so undelivered final answers land first.
    writer_failure: StdMutex<Option<DisconnectReason>>,
}

enum ClientOutboundFrame {
    Contiguous(Zeroizing<Vec<u8>>),
    ArtifactPut(uds_codec::ZeroizingEncodedArtifactPutFrame),
}

impl From<Zeroizing<Vec<u8>>> for ClientOutboundFrame {
    fn from(bytes: Zeroizing<Vec<u8>>) -> Self {
        Self::Contiguous(bytes)
    }
}

impl Shared {
    /// The recorded death reason. A correlation that was dropped while the
    /// connection still reads as live is a local close — never a hang and
    /// never an untyped error.
    fn disconnect_reason(&self) -> DisconnectReason {
        match &*self.state.borrow() {
            ConnectionState::Disconnected(reason) => reason.clone(),
            ConnectionState::Connected => DisconnectReason::Closed,
        }
    }

    /// Records a writer death WITHOUT flipping the connection state —
    /// the reader drains buffered frames to EOF first (see `run_writer`).
    fn defer_writer_failure(&self, reason: DisconnectReason) {
        if let Ok(mut slot) = self.writer_failure.lock() {
            slot.get_or_insert(reason);
        }
    }

    fn record_lost_event(&self) {
        self.lost_events
            .send_modify(|lost| *lost = lost.saturating_add(1));
    }

    /// First disconnect reason wins; pending requests observe it by drop.
    fn fail(&self, reason: DisconnectReason) {
        let mut first = false;
        self.state.send_if_modified(|state| {
            if matches!(state, ConnectionState::Connected) {
                *state = ConnectionState::Disconnected(reason.clone());
                first = true;
                true
            } else {
                false
            }
        });
        if first && let Ok(mut pending) = self.pending.lock() {
            pending.clear();
        }
    }
}

/// One live negotiated connection. See the module charter.
pub struct RpcClient {
    shared: Arc<Shared>,
    outbound: mpsc::Sender<ClientOutboundFrame>,
    events: StdMutex<Option<mpsc::Receiver<WireFrame>>>,
    outbound_limit: usize,
    encoding: WireEncoding,
    request_timeout: Duration,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    /// Transport shutdown authority retained outside the reader/writer tasks.
    /// Unix uses it for a peer-visible `shutdown(2)` without another executor
    /// poll. On Windows it only empties the Tokio pipe slot; the runtime must
    /// subsequently poll cancelled IOCP operations before the handle closes.
    shutdown: haider_platform::IpcShutdown,
    /// The handshake the connection negotiated on. Retained so a client that
    /// only receives the `RpcClient` (the TUI's `run_live`) can still gate
    /// its affordances on what the daemon ADVERTISED — report §4.1's
    /// "hide/disable only the methods whose feature is absent".
    welcome: Welcome,
    /// Kernel-authenticated identity captured before the stream was split.
    peer_credentials: PeerCredentials,
}

impl RpcClient {
    fn start(
        stream: IpcStream,
        decoder: uds_codec::Decoder,
        leftovers: VecDeque<WireFrame>,
        config: &ClientConfig,
        welcome: &Welcome,
        encoding: WireEncoding,
        peer_credentials: PeerCredentials,
    ) -> std::io::Result<Self> {
        let shutdown = haider_platform::shutdown_handle(&stream)?;
        let (state, _) = watch::channel(ConnectionState::Connected);
        let (lost_events, _) = watch::channel(0);
        let shared = Arc::new(Shared {
            pending: StdMutex::new(HashMap::new()),
            next_request: AtomicU64::new(1),
            acked_pong: AtomicU64::new(0),
            lost_events,
            writer_failure: StdMutex::new(None),
            state,
        });
        // The daemon never accepts a frame larger than its own advertised
        // limit; clamp outbound encoding to it.
        let outbound_limit = config.frame_limit.min(welcome.frame_limit as usize).max(1);
        let (outbound, outbound_rx) = mpsc::channel::<ClientOutboundFrame>(OUTBOUND_CAPACITY);
        let (events_tx, events_rx) = match config.connection_usage {
            ConnectionUsage::LongLived => {
                let (sender, receiver) = mpsc::channel::<WireFrame>(EVENT_CAPACITY);
                (Some(sender), Some(receiver))
            }
            ConnectionUsage::OneShot => (None, None),
        };
        let (reader_half, writer_half) = haider_platform::split(stream);

        let mut tasks = vec![
            tokio::spawn(run_reader(
                reader_half,
                decoder,
                leftovers,
                Arc::clone(&shared),
                events_tx,
                outbound.clone(),
                NegotiatedCodec {
                    frame_limit: outbound_limit,
                    encoding,
                },
            )),
            tokio::spawn(run_writer(writer_half, outbound_rx, Arc::clone(&shared))),
        ];
        if config.connection_usage == ConnectionUsage::LongLived {
            tasks.push(tokio::spawn(run_heartbeat(
                Arc::clone(&shared),
                outbound.clone(),
                outbound_limit,
                encoding,
                config.ping_interval,
                config.pong_deadline,
            )));
        }
        Ok(Self {
            shared,
            outbound,
            events: StdMutex::new(events_rx),
            outbound_limit,
            encoding,
            request_timeout: config.request_timeout,
            tasks,
            shutdown,
            welcome: welcome.clone(),
            peer_credentials,
        })
    }

    /// The negotiated handshake (daemon version, granted capabilities, and
    /// the advertised feature set).
    #[must_use]
    pub fn welcome(&self) -> &Welcome {
        &self.welcome
    }

    /// Kernel-authenticated identity of the peer serving this live connection.
    #[must_use]
    pub fn peer_credentials(&self) -> PeerCredentials {
        self.peer_credentials
    }

    /// Current lifecycle state.
    pub fn state(&self) -> ConnectionState {
        self.shared.state.borrow().clone()
    }

    /// Waits until the connection is dead and returns why — the caller's
    /// reconnect cue.
    pub async fn disconnected(&self) -> DisconnectReason {
        let mut receiver = self.shared.state.subscribe();
        loop {
            if let ConnectionState::Disconnected(reason) = receiver.borrow_and_update().clone() {
                return reason;
            }
            if receiver.changed().await.is_err() {
                return DisconnectReason::Closed;
            }
        }
    }

    /// Number of uncorrelated frames dropped under event backpressure.
    pub fn lost_events(&self) -> u64 {
        *self.shared.lost_events.borrow()
    }

    pub(crate) fn health_watch(&self) -> ClientHealthWatch {
        ClientHealthWatch {
            state: self.shared.state.subscribe(),
            lost_events: self.shared.lost_events.subscribe(),
            initial: true,
        }
    }

    /// Takes the uncorrelated frame stream (events, caught-up markers,
    /// roster/surface notifications, injection, and lag/drain notices).
    /// Yields `None` once for a second caller.
    pub fn take_events(&self) -> Option<mpsc::Receiver<WireFrame>> {
        self.events.lock().ok().and_then(|mut slot| slot.take())
    }

    /// Returns an event receiver taken speculatively by a higher-level attach
    /// helper when the correlated attach request was rejected. A successful
    /// attachment keeps ownership of the receiver instead.
    pub(crate) fn restore_events(
        &self,
        receiver: mpsc::Receiver<WireFrame>,
    ) -> Result<(), mpsc::Receiver<WireFrame>> {
        let Ok(mut slot) = self.events.lock() else {
            return Err(receiver);
        };
        if slot.is_some() {
            return Err(receiver);
        }
        *slot = Some(receiver);
        Ok(())
    }

    /// Sends one correlated request and awaits its response body.
    ///
    /// Exactly [`Self::begin_request`] followed by [`PendingResponse::wait`]:
    /// there is ONE piece of correlation logic in this file, because a second
    /// copy is how the pending-lock ordering below silently drifts back to
    /// the orphaning shape it was fixed out of.
    pub async fn request(&self, body: RequestBody) -> Result<ResponseBody, ClientError> {
        self.begin_request(body).await?.wait().await
    }

    /// Sends the same receipted durable menu-answer frame used by interactive
    /// clients and waits for its correlated resolution receipt.
    pub async fn answer_menu(
        &self,
        request: MenuAnswerRequest,
    ) -> Result<ResponseBody, ClientError> {
        self.begin_correlated_frame(move |request_id| WireFrame::MenuAnswer {
            request_id: Some(request_id),
            command_id: request.command_id,
            session_id: request.session_id,
            menu_id: request.menu_id,
            request_seq: request.request_seq,
            worker_generation: request.worker_generation,
            option_key: request.option_key,
            option_index: request.option_index,
            input: request.input,
        })
        .await?
        .wait()
        .await
    }

    /// Registers the correlation, writes the frame, and hands back the
    /// handle that resolves when the response lands.
    ///
    /// THE SEND HALF IS THE ORDERED HALF. A caller that must put two frames
    /// on the wire in a fixed order — `session.detach` before the
    /// `session.attach` that reuses its slot — awaits `begin_request` inline
    /// and moves only the returned [`PendingResponse`] onto a task. Awaiting
    /// the whole of [`Self::request`] on a task instead lets two requests
    /// race to `outbound.send`, and the daemon then sees the attach first and
    /// rejects it at `max_attachments_per_connection` (review W3c3 P1-3).
    pub async fn begin_request(&self, body: RequestBody) -> Result<PendingResponse, ClientError> {
        if let Some(feature) = body.additive_shape_feature()
            && !self.welcome.features.contains(feature)
        {
            return Err(ClientError::MissingFeature(feature));
        }
        self.begin_correlated_frame(|request_id| WireFrame::Request { request_id, body })
            .await
    }

    /// Sends one `artifact.put` from a stable private base64 snapshot. The
    /// snapshot is retained by the caller across reconnects, while each
    /// attempt borrows it into small encoded frame segments instead of
    /// cloning it into a `RequestBody` and serializer buffer.
    pub(crate) async fn request_artifact_put(
        &self,
        data_base64: Arc<Zeroizing<String>>,
    ) -> Result<ResponseBody, ClientError> {
        self.begin_correlated_encoded(move |request_id| {
            uds_codec::encode_artifact_put_request_parts_with(
                request_id,
                data_base64,
                self.outbound_limit,
                self.encoding,
            )
            .map(ClientOutboundFrame::ArtifactPut)
            .map_err(ClientError::Encode)
        })
        .await?
        .wait()
        .await
    }

    /// Registers correlation for a non-`Request` frame carrying an optional
    /// `request_id`, writes it, and returns its response handle.
    ///
    /// `MenuAnswer` is the sole current caller. Keeping it on the same pending
    /// map as ordinary requests makes response loss a reconnect cue instead
    /// of treating successful socket enqueue as durable menu resolution.
    pub(crate) async fn begin_correlated_frame(
        &self,
        build: impl FnOnce(RequestId) -> WireFrame,
    ) -> Result<PendingResponse, ClientError> {
        self.begin_correlated_encoded(move |request_id| {
            let frame = build(request_id.clone());
            uds_codec::encode_zeroizing_with(&frame, self.outbound_limit, self.encoding)
                .map(ClientOutboundFrame::from)
                .map_err(ClientError::Encode)
        })
        .await
    }

    async fn begin_correlated_encoded(
        &self,
        encode: impl FnOnce(&RequestId) -> Result<ClientOutboundFrame, ClientError>,
    ) -> Result<PendingResponse, ClientError> {
        // One deadline covers admission to the bounded writer queue and the
        // correlated response. No phase may reset the request's time budget.
        let request_timeout = self.request_timeout;
        let deadline = Instant::now() + request_timeout;
        let id = format!(
            "req-{}",
            self.shared.next_request.fetch_add(1, Ordering::Relaxed)
        );
        let request_id = RequestId::new(id.clone());
        // Sensitive encode path for EVERY outbound frame: `vault.stage`
        // bodies carry raw secrets, and uniform hygiene is cheaper than a
        // per-method split — the intermediate JSON body is scrubbed inside
        // the codec and the framed buffer zeroizes when the writer drops it.
        let bytes = encode(&request_id)?;
        let (sender, receiver) = oneshot::channel();
        {
            let Ok(mut pending) = self.shared.pending.lock() else {
                return Err(ClientError::Disconnected(DisconnectReason::Closed));
            };
            // The state check sits INSIDE the pending lock: `fail` flips the
            // state before its one-time clear takes this same lock, so either
            // the flip is visible here (typed error, nothing inserted) or the
            // sender is inserted before the clear runs and the clear drops it
            // — the receiver below then resolves with the typed disconnect.
            // A pre-lock check leaves a window where the clear completes
            // between check and insert, orphaning the sender forever (W3c2
            // review finding 1).
            if let ConnectionState::Disconnected(reason) = self.state() {
                return Err(ClientError::Disconnected(reason));
            }
            pending.insert(id.clone(), sender);
        }
        match tokio::time::timeout_at(deadline, self.outbound.send(bytes)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                if let Ok(mut pending) = self.shared.pending.lock() {
                    pending.remove(&id);
                }
                return Err(ClientError::Disconnected(self.disconnect_reason()));
            }
            Err(_) => {
                let reason = DisconnectReason::RequestTimeout {
                    request_id: id.clone(),
                    timeout: request_timeout,
                };
                self.shared.fail(reason.clone());
                return Err(ClientError::Disconnected(reason));
            }
        }
        Ok(PendingResponse {
            receiver,
            shared: Arc::clone(&self.shared),
            request_id: id,
            deadline,
            timeout: request_timeout,
        })
    }

    /// Sends one uncorrelated frame (`MenuAnswer` is the intended user).
    pub async fn send_frame(&self, frame: WireFrame) -> Result<(), ClientError> {
        let bytes = uds_codec::encode_zeroizing_with(&frame, self.outbound_limit, self.encoding)
            .map(ClientOutboundFrame::from)
            .map_err(ClientError::Encode)?;
        self.outbound
            .send(bytes)
            .await
            .map_err(|_| ClientError::Disconnected(self.disconnect_reason()))
    }

    /// Closes the connection locally and reports what the transport proved.
    ///
    /// The call is idempotent, and a later [`Self::disconnected`] returns the
    /// first recorded reason. Unix notifies the peer synchronously. Windows
    /// returns [`ClientCloseOutcome::LocalSlotEmptiedOnly`]; callers at a
    /// synchronous boundary must poll the owning Tokio runtime so cancelled
    /// IOCP completions can release the named-pipe handle.
    pub fn close(&self) -> ClientCloseOutcome {
        self.shared.fail(DisconnectReason::Closed);
        let outcome = match self.shutdown.request() {
            Ok(haider_platform::IpcShutdownOutcome::PeerNotified) => {
                ClientCloseOutcome::PeerNotified
            }
            Ok(haider_platform::IpcShutdownOutcome::PeerAlreadyGone) => {
                ClientCloseOutcome::AlreadyClosed
            }
            Ok(haider_platform::IpcShutdownOutcome::LocalSlotEmptiedOnly) => {
                ClientCloseOutcome::LocalSlotEmptiedOnly
            }
            Ok(haider_platform::IpcShutdownOutcome::AlreadyRequested) => {
                ClientCloseOutcome::AlreadyClosed
            }
            Err(error) => ClientCloseOutcome::ShutdownFailed(error),
        };
        for task in &self.tasks {
            task.abort();
        }
        outcome
    }

    fn disconnect_reason(&self) -> DisconnectReason {
        self.shared.disconnect_reason()
    }
}

/// A correlated request whose frame is ALREADY on the wire, waiting only for
/// its response.
///
/// This is the half of a request that may be moved onto a task. Holding it
/// keeps the correlation registered, so the response cannot be lost by the
/// caller deferring its wait; dropping it abandons the wait, and the reply
/// is then discarded when it arrives (the pending entry is removed either
/// way, by the reader or by the connection's one-time clear).
pub struct PendingResponse {
    receiver: oneshot::Receiver<ResponseBody>,
    shared: Arc<Shared>,
    request_id: String,
    deadline: Instant,
    timeout: Duration,
}

impl PendingResponse {
    /// Awaits the daemon's answer.
    ///
    /// A correlation dropped by a dying connection resolves as the typed
    /// [`DisconnectReason`] — the caller's reconnect cue — never as a hang.
    pub async fn wait(self) -> Result<ResponseBody, ClientError> {
        let Self {
            receiver,
            shared,
            request_id,
            deadline,
            timeout,
        } = self;
        match tokio::time::timeout_at(deadline, receiver).await {
            Ok(Ok(body)) => Ok(body),
            Ok(Err(_)) => Err(ClientError::Disconnected(shared.disconnect_reason())),
            Err(_) => {
                let reason = DisconnectReason::RequestTimeout {
                    request_id,
                    timeout,
                };
                shared.fail(reason.clone());
                Err(ClientError::Disconnected(reason))
            }
        }
    }
}

impl Drop for RpcClient {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

async fn run_reader(
    mut reader: IpcReadHalf,
    mut decoder: uds_codec::Decoder,
    leftovers: VecDeque<WireFrame>,
    shared: Arc<Shared>,
    events: Option<mpsc::Sender<WireFrame>>,
    outbound: mpsc::Sender<ClientOutboundFrame>,
    codec: NegotiatedCodec,
) {
    let mut state = shared.state.subscribe();
    for frame in leftovers {
        route_frame(
            frame,
            &shared,
            events.as_ref(),
            &outbound,
            codec.frame_limit,
            codec.encoding,
        )
        .await;
    }
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        tokio::select! {
            changed = state.changed() => {
                if changed.is_err()
                    || matches!(*state.borrow(), ConnectionState::Disconnected(_))
                {
                    return;
                }
            }
            read = reader.read(&mut buffer) => {
                let read = match read {
                    Ok(0) => {
                        shared.fail(DisconnectReason::PeerClosed);
                        return;
                    }
                    Ok(read) => read,
                    Err(error) => {
                        shared.fail(DisconnectReason::Io(error.to_string()));
                        return;
                    }
                };
                let batch = decoder.push(&buffer[..read]);
                for frame in batch.frames {
                    route_frame(
                        frame,
                        &shared,
                        events.as_ref(),
                        &outbound,
                        codec.frame_limit,
                        codec.encoding,
                    )
                    .await;
                }
                if let Some(error) = batch.error {
                    shared.fail(DisconnectReason::Protocol(error.to_string()));
                    return;
                }
            }
        }
    }
}

async fn route_frame(
    frame: WireFrame,
    shared: &Arc<Shared>,
    events: Option<&mpsc::Sender<WireFrame>>,
    outbound: &mpsc::Sender<ClientOutboundFrame>,
    outbound_limit: usize,
    encoding: WireEncoding,
) {
    match frame {
        WireFrame::Response { request_id, body } => {
            let sender = shared
                .pending
                .lock()
                .ok()
                .and_then(|mut pending| pending.remove(request_id.as_str()));
            if let Some(sender) = sender {
                let _ = sender.send(body);
            }
        }
        WireFrame::Pong { nonce } => {
            shared.acked_pong.fetch_max(nonce, Ordering::Relaxed);
        }
        WireFrame::Ping { nonce } => {
            // The daemon does not ping today; answer anyway (tolerance).
            if let Ok(bytes) = uds_codec::encode_zeroizing_with(
                &WireFrame::Pong { nonce },
                outbound_limit,
                encoding,
            ) {
                let _ = outbound.try_send(bytes.into());
            }
        }
        WireFrame::ProtocolError(error) => {
            let fatal = error.fatal;
            let forwarded = WireFrame::ProtocolError(error.clone());
            if events.is_some_and(|events| events.try_send(forwarded).is_err()) {
                shared.record_lost_event();
            }
            if fatal {
                shared.fail(DisconnectReason::Fatal(error));
            }
        }
        binding @ WireFrame::ResidentSessionBinding { .. } => {
            if events.is_some_and(|events| events.try_send(binding).is_err()) {
                shared.record_lost_event();
                shared.fail(DisconnectReason::Protocol(
                    "resident session binding could not reach the authoritative event channel"
                        .into(),
                ));
            }
        }
        other => {
            // Uncorrelated traffic must never block response correlation:
            // a full event channel drops the frame and the consumer's
            // seq-cursor reattach discipline recovers the gap.
            if events.is_some_and(|events| events.try_send(other).is_err()) {
                shared.record_lost_event();
            }
        }
    }
}

async fn run_writer(
    mut writer: IpcWriteHalf,
    mut outbound: mpsc::Receiver<ClientOutboundFrame>,
    shared: Arc<Shared>,
) {
    let mut state = shared.state.subscribe();
    loop {
        tokio::select! {
            changed = state.changed() => {
                if changed.is_err()
                    || matches!(*state.borrow(), ConnectionState::Disconnected(_))
                {
                    break;
                }
            }
            frame = outbound.recv() => {
                let Some(bytes) = frame else {
                    shared.fail(DisconnectReason::Closed);
                    break;
                };
                if let Err(error) = write_client_frame(&mut writer, &bytes).await {
                    // W9b review fix: the read side may still hold
                    // undelivered final answers (a daemon that answers and
                    // then closes — crash after commit, or a graceful close
                    // right after the last response; a heartbeat ping takes
                    // the EPIPE first). Deferring the failure lets the
                    // reader drain to EOF before pending waiters observe
                    // the death; the pong deadline bounds the half-open
                    // case where the peer never closes its side.
                    shared.defer_writer_failure(DisconnectReason::Io(error.to_string()));
                    break;
                }
                // `bytes` (Zeroizing) drops here: the encoded frame —
                // including any staged secret — is wiped after the write.
            }
        }
    }
    let _ = writer.shutdown().await;
}

async fn write_client_frame(
    writer: &mut IpcWriteHalf,
    frame: &ClientOutboundFrame,
) -> std::io::Result<()> {
    match frame {
        ClientOutboundFrame::Contiguous(bytes) => writer.write_all(bytes.as_slice()).await,
        ClientOutboundFrame::ArtifactPut(frame) => {
            let segments: [&[u8]; 4] = [
                frame.prefix(),
                frame.head(),
                frame.data_base64(),
                frame.tail(),
            ];
            let mut segment = 0;
            let mut offset = 0;
            while segment < segments.len() {
                while segment < segments.len() && offset == segments[segment].len() {
                    segment += 1;
                    offset = 0;
                }
                if segment == segments.len() {
                    break;
                }
                let slices = [
                    IoSlice::new(if segment == 0 {
                        &segments[0][offset..]
                    } else {
                        &[]
                    }),
                    IoSlice::new(if segment <= 1 {
                        &segments[1][if segment == 1 { offset } else { 0 }..]
                    } else {
                        &[]
                    }),
                    IoSlice::new(if segment <= 2 {
                        &segments[2][if segment == 2 { offset } else { 0 }..]
                    } else {
                        &[]
                    }),
                    IoSlice::new(if segment <= 3 {
                        &segments[3][if segment == 3 { offset } else { 0 }..]
                    } else {
                        &[]
                    }),
                ];
                let written = writer.write_vectored(&slices[segment..]).await?;
                if written == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "failed to write segmented artifact.put frame",
                    ));
                }
                let mut remaining = written;
                while remaining > 0 && segment < segments.len() {
                    let available = segments[segment].len().saturating_sub(offset);
                    let consumed = remaining.min(available);
                    offset += consumed;
                    remaining -= consumed;
                    if offset == segments[segment].len() {
                        segment += 1;
                        offset = 0;
                    }
                }
            }
            Ok(())
        }
    }
}

async fn run_heartbeat(
    shared: Arc<Shared>,
    outbound: mpsc::Sender<ClientOutboundFrame>,
    outbound_limit: usize,
    encoding: WireEncoding,
    ping_interval: Duration,
    pong_deadline: Duration,
) {
    let mut state = shared.state.subscribe();
    let mut ticker = tokio::time::interval_at(Instant::now() + ping_interval, ping_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut next_nonce: u64 = 1;
    let mut unacked: VecDeque<(u64, Instant)> = VecDeque::new();
    loop {
        tokio::select! {
            changed = state.changed() => {
                if changed.is_err()
                    || matches!(*state.borrow(), ConnectionState::Disconnected(_))
                {
                    return;
                }
            }
            _ = ticker.tick() => {
                let acked = shared.acked_pong.load(Ordering::Relaxed);
                while unacked.front().is_some_and(|(nonce, _)| *nonce <= acked) {
                    unacked.pop_front();
                }
                if unacked
                    .front()
                    .is_some_and(|(_, sent)| sent.elapsed() >= pong_deadline)
                {
                    shared.fail(DisconnectReason::PongTimeout);
                    return;
                }
                let nonce = next_nonce;
                next_nonce = next_nonce.saturating_add(1);
                if let Ok(bytes) = uds_codec::encode_zeroizing_with(
                    &WireFrame::Ping { nonce },
                    outbound_limit,
                    encoding,
                )
                {
                    // A full outbound queue is itself no-progress evidence;
                    // the unanswered ping deadline will catch it.
                    let _ = outbound.try_send(bytes.into());
                }
                unacked.push_back((nonce, Instant::now()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    /// MUTATION CHECK: move `ResidentSessionBinding` into the generic
    /// `other` arm. Expected runtime failure: a full event channel leaves the
    /// connection live after silently dropping the authoritative unbind.
    #[tokio::test]
    async fn resident_binding_event_overflow_disconnects_for_baseline_recovery() {
        let (state, _) = watch::channel(ConnectionState::Connected);
        let (lost_events, _) = watch::channel(0);
        let shared = Arc::new(Shared {
            pending: StdMutex::new(HashMap::new()),
            next_request: AtomicU64::new(1),
            acked_pong: AtomicU64::new(0),
            lost_events,
            writer_failure: StdMutex::new(None),
            state,
        });
        let (events, _event_rx) = mpsc::channel(1);
        events
            .try_send(WireFrame::Unknown)
            .expect("event channel is prefilled");
        let (outbound, _outbound_rx) = mpsc::channel(1);

        route_frame(
            WireFrame::ResidentSessionBinding {
                session_id: None,
                worker_generation: 9,
                binding_token: None,
            },
            &shared,
            Some(&events),
            &outbound,
            DEFAULT_FRAME_LIMIT,
            WireEncoding::Json,
        )
        .await;

        assert_eq!(*shared.lost_events.borrow(), 1);
        assert!(matches!(
            &*shared.state.borrow(),
            ConnectionState::Disconnected(DisconnectReason::Protocol(message))
                if message.contains("resident session binding")
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn client_health_watch_is_event_primary_with_a_typed_repair_deadline() {
        let (state, _) = watch::channel(ConnectionState::Connected);
        let (lost_events, _) = watch::channel(0_u64);
        let watcher = ClientHealthWatch {
            state: state.subscribe(),
            lost_events: lost_events.subscribe(),
            initial: true,
        };

        let (watcher, initial) = watcher
            .wait("client health fixture", Duration::from_secs(30))
            .await;
        assert!(matches!(initial, ClientHealthWait::Changed(_)));
        assert_eq!(initial.snapshot().lost_events, 0);

        lost_events.send_modify(|lost| *lost = 1);
        let (watcher, changed) = watcher
            .wait("client health fixture", Duration::from_secs(30))
            .await;
        assert_eq!(changed.snapshot().lost_events, 1);

        let (_watcher, repair) = watcher
            .wait("client health fixture", Duration::from_secs(30))
            .await;
        let timeout = repair
            .repair_timeout()
            .expect("quiet health watch reaches its typed repair deadline");
        assert_eq!(timeout.operation(), "client health fixture");
        assert_eq!(timeout.limit(), Duration::from_secs(30));
        assert_eq!(repair.snapshot().state, ConnectionState::Connected);
        assert_eq!(repair.snapshot().lost_events, 1);
    }

    #[tokio::test]
    async fn coalesced_json_welcome_and_msgpack_frame_switches_at_frame_boundary() {
        let welcome = Welcome {
            protocol: WIRE_PROTOCOL_VERSION,
            instance_id: "coalesced-instance".into(),
            daemon_generation: 7,
            frame_limit: DEFAULT_FRAME_LIMIT as u32,
            profile_id: "coalesced-profile".into(),
            daemon_version: "test".into(),
            lifecycle_phase: haider_rpc::LifecyclePhase::Ready,
            capabilities_granted: CapabilitySet::from([Capability::View]),
            features: Default::default(),
            user_command_withheld: false,
            encoding: Some("msgpack".into()),
        };
        let draining = WireFrame::ServerDraining {
            reason: "test drain".into(),
            instance_id: welcome.instance_id.clone(),
            daemon_generation: welcome.daemon_generation,
            deadline_unix_ms: 1234,
        };
        let mut bytes =
            uds_codec::encode(&WireFrame::Welcome(welcome.clone()), DEFAULT_FRAME_LIMIT)
                .expect("encode JSON Welcome");
        bytes.extend(
            uds_codec::encode_with(&draining, DEFAULT_FRAME_LIMIT, WireEncoding::MessagePack)
                .expect("encode MessagePack notice"),
        );

        // One peer write deliberately makes the JSON handshake and first
        // MessagePack frame available to the reader as a single byte stream.
        let (mut peer, mut reader) = tokio::io::duplex(bytes.len());
        peer.write_all(&bytes)
            .await
            .expect("write coalesced stream");

        let (decoded_welcome, _decoder, leftovers) =
            read_handshake(&mut reader, DEFAULT_FRAME_LIMIT)
                .await
                .expect("coalesced handshake succeeds");
        assert_eq!(decoded_welcome, welcome);
        assert_eq!(leftovers, VecDeque::from([draining]));
    }
}
