//! Reusable UDS RPC client (report §6.2).
//!
//! One [`RpcClient`] owns one negotiated connection: a bounded writer task, a
//! reader task that correlates `Response` frames to pending requests by
//! `RequestId`, an R9 heartbeat task (`Ping` every 15 s; a ping unmatched for
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
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use haider_rpc::{
    Capability, CapabilitySet, ClientKind, CodecError, DEFAULT_FRAME_LIMIT, Hello, ProtocolError,
    RequestBody, RequestId, ResponseBody, WIRE_PROTOCOL_VERSION, Welcome, WireFrame, uds_codec,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
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

const OUTBOUND_CAPACITY: usize = 64;
const EVENT_CAPACITY: usize = 256;

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
    pub ping_interval: Duration,
    pub pong_deadline: Duration,
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
            ping_interval: PING_INTERVAL,
            pong_deadline: PONG_DEADLINE,
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
        }
    }
}

/// A correlated request failed.
#[derive(Debug)]
pub enum ClientError {
    /// The connection is (or became) dead; the reason is the reconnect cue.
    Disconnected(DisconnectReason),
    /// The request body could not be encoded within the negotiated limit.
    Encode(CodecError),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disconnected(reason) => write!(formatter, "request failed: {reason}"),
            Self::Encode(error) => write!(formatter, "request could not be encoded: {error}"),
        }
    }
}

impl std::error::Error for ClientError {}

/// A successfully negotiated connection.
pub struct Connected {
    pub client: RpcClient,
    pub welcome: Welcome,
}

/// Dials the endpoint and completes the `Hello`/`Welcome` handshake.
pub async fn connect(path: &Path, config: ClientConfig) -> Result<Connected, ConnectError> {
    let mut stream = match UnixStream::connect(path).await {
        Ok(stream) => stream,
        Err(error) => return Err(classify_connect_error(error)),
    };
    let hello = WireFrame::Hello(Hello {
        protocol_min: WIRE_PROTOCOL_VERSION,
        protocol_max: WIRE_PROTOCOL_VERSION,
        client_name: config.client_name.clone(),
        client_version: config.client_version.clone(),
        client_instance_id: config.client_instance_id.clone(),
        client_kind: config.client_kind,
        capabilities_requested: config.capabilities.clone(),
        max_receive_frame: u32::try_from(config.frame_limit).unwrap_or(u32::MAX),
    });
    let bytes = uds_codec::encode(&hello, config.frame_limit).map_err(ConnectError::Frame)?;
    let handshake = async {
        stream.write_all(&bytes).await.map_err(ConnectError::Io)?;
        let mut decoder = uds_codec::Decoder::new_zeroizing(config.frame_limit);
        let mut buffer = [0_u8; 16 * 1024];
        let mut leftovers = VecDeque::new();
        loop {
            let read = stream.read(&mut buffer).await.map_err(ConnectError::Io)?;
            if read == 0 {
                return Err(ConnectError::ClosedDuringHandshake);
            }
            let batch = decoder.push(&buffer[..read]);
            let mut frames = batch.frames.into_iter();
            let first = frames.next();
            match first {
                Some(WireFrame::Welcome(welcome)) => {
                    leftovers.extend(frames);
                    if let Some(error) = batch.error {
                        return Err(ConnectError::Frame(error));
                    }
                    return Ok((welcome, decoder, leftovers));
                }
                Some(WireFrame::ProtocolError(error)) => {
                    return Err(ConnectError::Rejected(error));
                }
                Some(_) => return Err(ConnectError::UnexpectedFrame),
                None => {
                    if let Some(error) = batch.error {
                        return Err(ConnectError::Frame(error));
                    }
                }
            }
        }
    };
    let (welcome, decoder, leftovers) =
        match tokio::time::timeout(config.handshake_timeout, handshake).await {
            Ok(result) => result?,
            Err(_) => return Err(ConnectError::HandshakeTimeout),
        };
    let client = RpcClient::start(stream, decoder, leftovers, &config, &welcome);
    Ok(Connected { client, welcome })
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

struct Shared {
    pending: StdMutex<HashMap<String, oneshot::Sender<ResponseBody>>>,
    next_request: AtomicU64,
    /// Highest ping nonce a `Pong` has acknowledged.
    acked_pong: AtomicU64,
    /// Events dropped because the event channel was full (the consumer's
    /// reattach-by-cursor discipline recovers; see R9's cursor law).
    lost_events: AtomicU64,
    state: watch::Sender<ConnectionState>,
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
    outbound: mpsc::Sender<Zeroizing<Vec<u8>>>,
    events: StdMutex<Option<mpsc::Receiver<WireFrame>>>,
    outbound_limit: usize,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    /// The handshake the connection negotiated on. Retained so a client that
    /// only receives the `RpcClient` (the TUI's `run_live`) can still gate
    /// its affordances on what the daemon ADVERTISED — report §4.1's
    /// "hide/disable only the methods whose feature is absent".
    welcome: Welcome,
}

impl RpcClient {
    fn start(
        stream: UnixStream,
        decoder: uds_codec::Decoder,
        leftovers: VecDeque<WireFrame>,
        config: &ClientConfig,
        welcome: &Welcome,
    ) -> Self {
        let (state, _) = watch::channel(ConnectionState::Connected);
        let shared = Arc::new(Shared {
            pending: StdMutex::new(HashMap::new()),
            next_request: AtomicU64::new(1),
            acked_pong: AtomicU64::new(0),
            lost_events: AtomicU64::new(0),
            state,
        });
        // The daemon never accepts a frame larger than its own advertised
        // limit; clamp outbound encoding to it.
        let outbound_limit = config.frame_limit.min(welcome.frame_limit as usize).max(1);
        let (outbound, outbound_rx) = mpsc::channel::<Zeroizing<Vec<u8>>>(OUTBOUND_CAPACITY);
        let (events_tx, events_rx) = mpsc::channel::<WireFrame>(EVENT_CAPACITY);
        let (reader_half, writer_half) = stream.into_split();

        let tasks = vec![
            tokio::spawn(run_reader(
                reader_half,
                decoder,
                leftovers,
                Arc::clone(&shared),
                events_tx,
                outbound.clone(),
                outbound_limit,
            )),
            tokio::spawn(run_writer(writer_half, outbound_rx, Arc::clone(&shared))),
            tokio::spawn(run_heartbeat(
                Arc::clone(&shared),
                outbound.clone(),
                outbound_limit,
                config.ping_interval,
                config.pong_deadline,
            )),
        ];
        Self {
            shared,
            outbound,
            events: StdMutex::new(Some(events_rx)),
            outbound_limit,
            tasks,
            welcome: welcome.clone(),
        }
    }

    /// The negotiated handshake (daemon version, granted capabilities, and
    /// the advertised feature set).
    #[must_use]
    pub fn welcome(&self) -> &Welcome {
        &self.welcome
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
        self.shared.lost_events.load(Ordering::Relaxed)
    }

    /// Takes the uncorrelated frame stream (events, caught-up markers,
    /// lag/drain notices). Yields `None` once for a second caller.
    pub fn take_events(&self) -> Option<mpsc::Receiver<WireFrame>> {
        self.events.lock().ok().and_then(|mut slot| slot.take())
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
        self.begin_correlated_frame(|request_id| WireFrame::Request { request_id, body })
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
        let id = format!(
            "req-{}",
            self.shared.next_request.fetch_add(1, Ordering::Relaxed)
        );
        let frame = build(RequestId::new(id.clone()));
        // Sensitive encode path for EVERY outbound frame: `vault.stage`
        // bodies carry raw secrets, and uniform hygiene is cheaper than a
        // per-method split — the intermediate JSON body is scrubbed inside
        // the codec and the framed buffer zeroizes when the writer drops it.
        let bytes = uds_codec::encode_zeroizing(&frame, self.outbound_limit)
            .map_err(ClientError::Encode)?;
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
        if self.outbound.send(bytes).await.is_err() {
            if let Ok(mut pending) = self.shared.pending.lock() {
                pending.remove(&id);
            }
            return Err(ClientError::Disconnected(self.disconnect_reason()));
        }
        Ok(PendingResponse {
            receiver,
            shared: Arc::clone(&self.shared),
        })
    }

    /// Sends one uncorrelated frame (`MenuAnswer` is the intended user).
    pub async fn send_frame(&self, frame: WireFrame) -> Result<(), ClientError> {
        let bytes = uds_codec::encode_zeroizing(&frame, self.outbound_limit)
            .map_err(ClientError::Encode)?;
        self.outbound
            .send(bytes)
            .await
            .map_err(|_| ClientError::Disconnected(self.disconnect_reason()))
    }

    /// Closes the connection locally. Idempotent; a later
    /// [`Self::disconnected`] returns the first recorded reason.
    pub fn close(&self) {
        self.shared.fail(DisconnectReason::Closed);
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
}

impl PendingResponse {
    /// Awaits the daemon's answer.
    ///
    /// A correlation dropped by a dying connection resolves as the typed
    /// [`DisconnectReason`] — the caller's reconnect cue — never as a hang.
    pub async fn wait(self) -> Result<ResponseBody, ClientError> {
        let Self { receiver, shared } = self;
        match receiver.await {
            Ok(body) => Ok(body),
            Err(_) => Err(ClientError::Disconnected(shared.disconnect_reason())),
        }
    }
}

impl Drop for RpcClient {
    fn drop(&mut self) {
        self.shared.fail(DisconnectReason::Closed);
        for task in &self.tasks {
            task.abort();
        }
    }
}

async fn run_reader(
    mut reader: tokio::net::unix::OwnedReadHalf,
    mut decoder: uds_codec::Decoder,
    leftovers: VecDeque<WireFrame>,
    shared: Arc<Shared>,
    events: mpsc::Sender<WireFrame>,
    outbound: mpsc::Sender<Zeroizing<Vec<u8>>>,
    outbound_limit: usize,
) {
    let mut state = shared.state.subscribe();
    for frame in leftovers {
        route_frame(frame, &shared, &events, &outbound, outbound_limit).await;
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
                    route_frame(frame, &shared, &events, &outbound, outbound_limit).await;
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
    events: &mpsc::Sender<WireFrame>,
    outbound: &mpsc::Sender<Zeroizing<Vec<u8>>>,
    outbound_limit: usize,
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
            if let Ok(bytes) =
                uds_codec::encode_zeroizing(&WireFrame::Pong { nonce }, outbound_limit)
            {
                let _ = outbound.try_send(bytes);
            }
        }
        WireFrame::ProtocolError(error) => {
            let fatal = error.fatal;
            let forwarded = WireFrame::ProtocolError(error.clone());
            if events.try_send(forwarded).is_err() {
                shared.lost_events.fetch_add(1, Ordering::Relaxed);
            }
            if fatal {
                shared.fail(DisconnectReason::Fatal(error));
            }
        }
        other => {
            // Uncorrelated traffic must never block response correlation:
            // a full event channel drops the frame and the consumer's
            // seq-cursor reattach discipline recovers the gap.
            if events.try_send(other).is_err() {
                shared.lost_events.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

async fn run_writer(
    mut writer: tokio::net::unix::OwnedWriteHalf,
    mut outbound: mpsc::Receiver<Zeroizing<Vec<u8>>>,
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
                if let Err(error) = writer.write_all(&bytes).await {
                    shared.fail(DisconnectReason::Io(error.to_string()));
                    break;
                }
                // `bytes` (Zeroizing) drops here: the encoded frame —
                // including any staged secret — is wiped after the write.
            }
        }
    }
    let _ = writer.shutdown().await;
}

async fn run_heartbeat(
    shared: Arc<Shared>,
    outbound: mpsc::Sender<Zeroizing<Vec<u8>>>,
    outbound_limit: usize,
    ping_interval: Duration,
    pong_deadline: Duration,
) {
    let mut state = shared.state.subscribe();
    let mut ticker = tokio::time::interval(ping_interval);
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
                if let Ok(bytes) = uds_codec::encode_zeroizing(&WireFrame::Ping { nonce }, outbound_limit)
                {
                    // A full outbound queue is itself no-progress evidence;
                    // the unanswered ping deadline will catch it.
                    let _ = outbound.try_send(bytes);
                }
                unacked.push_back((nonce, Instant::now()));
            }
        }
    }
}
