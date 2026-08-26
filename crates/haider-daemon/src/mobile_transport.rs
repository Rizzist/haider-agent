//! Authenticated loopback transport for the companion Haider Android APK.
//!
//! The transport is deliberately opt-in and loopback-only. One actor owns
//! each authenticated TCP connection, correlates daemon capability requests,
//! and accepts the APK's push lane. The most recently authenticated APK wins.

#[path = "mobile_transport/chat_bridge.rs"]
mod chat_bridge;

use crate::{DaemonConfig, DaemonError, MonitorSourceHub, publish_sms_incoming};
use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use haider_protocol::effect::EffectClass;
use haider_protocol::mobile::{A11yNode, MobileAction, MobileOutput, Point4, SmsMessage};
use haider_tools::{MobileBackend, MobileCancelToken, MobileError, MobileResult};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::ffi::OsStr;
use std::fmt;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock, RwLock as StdRwLock, Weak};
use std::time::Duration;
use subtle::ConstantTimeEq as _;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex as AsyncMutex, broadcast, mpsc, oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};
use zeroize::{Zeroize, Zeroizing};

pub(crate) const MOBILE_APK_ENV: &str = "HAIDER_MOBILE_APK";
const MOBILE_HOME_ENV: &str = "HAIDER_HOME";
const TOKEN_FILE_NAME: &str = "mobile-token";
const TOKEN_RANDOM_BYTES: usize = 32;
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
const CONNECTION_COMMAND_CAPACITY: usize = 64;
const MAX_HANDSHAKES: usize = 8;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const BACKEND_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const DEFAULT_SWIPE_MS: u64 = 300;
const DEFAULT_SMS_LIMIT: i32 = 50;
const RECENT_SMS_CAPACITY: usize = 64;
const RECENT_SMS_MAX_BYTES: usize = 1024 * 1024;
const MAX_SMS_PUSH_ADDRESS_BYTES: usize = 1024;
const MAX_SMS_PUSH_BODY_BYTES: usize = 64 * 1024;
const MAX_GRANTED_CAPABILITIES: usize = 64;
const MAX_CAPABILITY_NAME_BYTES: usize = 128;
const MAX_CHAT_TEXT_BYTES: usize = 256 * 1024;
const MAX_SESSION_FIELD_BYTES: usize = 512;
const CHAT_OUTPUT_CAPACITY: usize = 128;
const CHAT_COMMAND_CAPACITY: usize = 4;
const MONITOR_CHAT_STREAM_CAPACITY: usize = 16;
const SERVER_CAPABILITIES: &[&str] = &[
    "a11y.snapshot",
    "a11y.tap",
    "a11y.swipe",
    "a11y.text",
    "screen.capture",
    "sms.list",
    "app.open",
];

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Envelope {
    id: i64,
    body: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MonitorChatStream {
    id: i64,
    text: String,
}

#[derive(Debug, Clone)]
enum ChatCommand {
    Send {
        text: String,
    },
    SessionConfig,
    SelectModel {
        provider: String,
        model: String,
        confirm_new_epoch: bool,
    },
    SelectEffort {
        effort: Option<String>,
        confirm_new_epoch: bool,
    },
}

impl ChatCommand {
    fn parse(body: &Value) -> Result<Self, MobileChatError> {
        let frame_type = body
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| MobileChatError::invalid("mobile request has no string type"))?;
        match frame_type {
            "chat.send" => {
                let text = body
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| MobileChatError::invalid("chat.send needs text"))?;
                if text.trim().is_empty() {
                    return Err(MobileChatError::invalid("chat text must not be empty"));
                }
                if text.len() > MAX_CHAT_TEXT_BYTES {
                    return Err(MobileChatError::invalid(
                        "chat text exceeds the 256 KiB limit",
                    ));
                }
                Ok(Self::Send { text: text.into() })
            }
            "session.config.get" => Ok(Self::SessionConfig),
            "session.select_model" => {
                let provider = bounded_session_field(body, "provider")?;
                let model = bounded_session_field(body, "model")?;
                Ok(Self::SelectModel {
                    provider,
                    model,
                    confirm_new_epoch: body
                        .get("confirmNewEpoch")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                })
            }
            "session.select_effort" => {
                let effort = match body.get("effort") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(effort))
                        if !effort.trim().is_empty() && effort.len() <= MAX_SESSION_FIELD_BYTES =>
                    {
                        Some(effort.clone())
                    }
                    Some(_) => {
                        return Err(MobileChatError::invalid(
                            "effort must be null or a non-empty bounded string",
                        ));
                    }
                };
                Ok(Self::SelectEffort {
                    effort,
                    confirm_new_epoch: body
                        .get("confirmNewEpoch")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                })
            }
            _ => Err(MobileChatError::invalid(format!(
                "unsupported mobile session request `{frame_type}`"
            ))),
        }
    }

    fn is_chat(&self) -> bool {
        matches!(self, Self::Send { .. })
    }
}

fn bounded_session_field(body: &Value, name: &str) -> Result<String, MobileChatError> {
    let value = body
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| MobileChatError::invalid(format!("{name} must be a string")))?;
    if value.trim().is_empty() || value.len() > MAX_SESSION_FIELD_BYTES {
        return Err(MobileChatError::invalid(format!(
            "{name} must be non-empty and at most {MAX_SESSION_FIELD_BYTES} bytes"
        )));
    }
    Ok(value.to_owned())
}

#[derive(Debug)]
enum ChatEvent {
    Delta {
        text: String,
        segment: &'static str,
    },
    Tool {
        call_id: String,
        name: String,
        summary: String,
        status: &'static str,
        result: Option<String>,
    },
    Status {
        text: String,
    },
    Done,
    ChatError(MobileChatError),
    SessionConfig(MobileSessionConfig),
    SessionError(MobileChatError),
}

impl ChatEvent {
    fn into_body(self) -> Result<Value, MobileChatError> {
        match self {
            Self::Delta { text, segment } => {
                Ok(json!({"type": "chat.delta", "text": text, "segment": segment}))
            }
            Self::Tool {
                call_id,
                name,
                summary,
                status,
                result,
            } => Ok(json!({
                "type": "chat.tool",
                "callId": call_id,
                "name": name,
                "summary": summary,
                "status": status,
                "result": result,
            })),
            Self::Status { text } => Ok(json!({"type": "chat.status", "text": text})),
            Self::Done => Ok(json!({"type": "chat.done"})),
            Self::ChatError(error) => Ok(json!({
                "type": "chat.error",
                "code": error.code,
                "message": error.message,
                "retryable": error.retryable,
            })),
            Self::SessionConfig(config) => {
                let mut body = serde_json::to_value(config).map_err(|error| {
                    MobileChatError::internal(format!(
                        "cannot encode mobile session config: {error}"
                    ))
                })?;
                body.as_object_mut()
                    .ok_or_else(|| {
                        MobileChatError::internal("mobile session config was not an object")
                    })?
                    .insert("type".into(), Value::String("session.config".into()));
                Ok(body)
            }
            Self::SessionError(error) => Ok(json!({
                "type": "session.error",
                "code": error.code,
                "message": error.message,
                "retryable": error.retryable,
            })),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MobileSessionConfig {
    catalog_revision: u64,
    catalog_available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    unavailable_reason: Option<String>,
    current: MobileSelection,
    providers: Vec<MobileProvider>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MobileSelection {
    session_id: String,
    provider: String,
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MobileProvider {
    id: String,
    enabled: bool,
    availability: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    availability_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_model: Option<String>,
    models: Vec<MobileModel>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MobileModel {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_window: Option<u64>,
    supported_efforts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_effort: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MobileChatError {
    code: String,
    message: String,
    retryable: bool,
}

impl MobileChatError {
    fn new(code: impl Into<String>, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable,
        }
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self::new("invalid_argument", message, false)
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new("internal", message, true)
    }

    fn daemon(error: impl fmt::Display) -> Self {
        Self::internal(error.to_string())
    }

    fn into_event(self, chat: bool) -> ChatEvent {
        if chat {
            ChatEvent::ChatError(self)
        } else {
            ChatEvent::SessionError(self)
        }
    }
}

impl fmt::Display for MobileChatError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for MobileChatError {}

#[derive(Clone)]
struct ChatResponder {
    id: i64,
    frames: mpsc::Sender<Envelope>,
}

impl ChatResponder {
    fn is_closed(&self) -> bool {
        self.frames.is_closed()
    }

    async fn wait_closed(&self) {
        self.frames.closed().await;
    }

    async fn send(&self, event: ChatEvent) -> Result<(), MobileChatError> {
        let body = event.into_body()?;
        self.frames
            .send(Envelope { id: self.id, body })
            .await
            .map_err(|_| MobileChatError::internal("mobile connection closed"))
    }
}

#[async_trait]
trait MobileChatBridge: Send + Sync {
    async fn handle(
        &self,
        command: ChatCommand,
        responder: ChatResponder,
    ) -> Result<(), MobileChatError>;
}

#[derive(Debug)]
enum TransportError {
    Io {
        operation: &'static str,
        message: String,
    },
    Protocol {
        message: String,
    },
}

impl TransportError {
    fn io(operation: &'static str, error: impl fmt::Display) -> Self {
        Self::Io {
            operation,
            message: error.to_string(),
        }
    }

    fn protocol(message: impl Into<String>) -> Self {
        Self::Protocol {
            message: message.into(),
        }
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { operation, message } => write!(formatter, "{operation}: {message}"),
            Self::Protocol { message } => formatter.write_str(message),
        }
    }
}

impl std::error::Error for TransportError {}

fn encode_frame(envelope: &Envelope) -> Result<Vec<u8>, TransportError> {
    let payload = serde_json::to_vec(envelope).map_err(|error| {
        TransportError::protocol(format!("cannot encode JSON envelope: {error}"))
    })?;
    if payload.is_empty() || payload.len() > MAX_FRAME_BYTES {
        return Err(TransportError::protocol(
            "outbound mobile frame exceeds the protocol limit",
        ));
    }
    let length = u32::try_from(payload.len())
        .map_err(|_| TransportError::protocol("mobile frame length does not fit u32"))?;
    let mut frame = Vec::with_capacity(payload.len() + 4);
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

async fn read_frame<R>(reader: &mut R) -> Result<Envelope, TransportError>
where
    R: AsyncRead + Unpin,
{
    let mut prefix = [0_u8; 4];
    reader
        .read_exact(&mut prefix)
        .await
        .map_err(|error| TransportError::io("read mobile frame header", error))?;
    let length = u32::from_be_bytes(prefix) as usize;
    if length == 0 {
        return Err(TransportError::protocol("empty mobile protocol frame"));
    }
    if length > MAX_FRAME_BYTES {
        return Err(TransportError::protocol(
            "mobile frame exceeds the 8 MiB protocol limit",
        ));
    }
    let mut payload = Zeroizing::new(vec![0_u8; length]);
    reader
        .read_exact(payload.as_mut_slice())
        .await
        .map_err(|error| TransportError::io("read mobile frame payload", error))?;
    serde_json::from_slice(payload.as_slice())
        .map_err(|error| TransportError::protocol(format!("invalid mobile JSON envelope: {error}")))
}

async fn write_frame<W>(writer: &mut W, envelope: &Envelope) -> Result<(), TransportError>
where
    W: AsyncWrite + Unpin,
{
    let mut frame = Zeroizing::new(encode_frame(envelope)?);
    let result = writer
        .write_all(frame.as_slice())
        .await
        .map_err(|error| TransportError::io("write mobile frame", error));
    frame.zeroize();
    result
}

fn constant_time_token_eq(expected: &[u8], candidate: &[u8]) -> bool {
    // The generated token has a fixed encoded length. Iterate for the longer
    // input and fold the length difference into the same accumulator: neither
    // a content mismatch nor a length mismatch returns early.
    let longest = expected.len().max(candidate.len());
    let mut difference = expected.len() ^ candidate.len();
    for index in 0..longest {
        let left = expected.get(index).copied().unwrap_or(0);
        let right = candidate.get(index).copied().unwrap_or(0);
        difference |= usize::from(left ^ right);
    }
    bool::from(difference.ct_eq(&0))
}

fn generate_token() -> Result<Zeroizing<String>, TransportError> {
    let mut random = Zeroizing::new([0_u8; TOKEN_RANDOM_BYTES]);
    getrandom::fill(random.as_mut_slice())
        .map_err(|error| TransportError::io("generate mobile authentication token", error))?;
    Ok(Zeroizing::new(URL_SAFE_NO_PAD.encode(random.as_slice())))
}

fn write_mobile_token(home: &Path, token: &str) -> Result<PathBuf, TransportError> {
    std::fs::create_dir_all(home)
        .map_err(|error| TransportError::io("create HAIDER_HOME for mobile token", error))?;
    let path = home.join(TOKEN_FILE_NAME);
    #[cfg(unix)]
    {
        write_mobile_token_unix(home, path, token)
    }
    #[cfg(not(unix))]
    {
        write_mobile_token_portable(path, token)
    }
}

#[cfg(unix)]
fn write_mobile_token_unix(
    home: &Path,
    path: PathBuf,
    token: &str,
) -> Result<PathBuf, TransportError> {
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

    let mut nonce = Zeroizing::new([0_u8; 8]);
    getrandom::fill(nonce.as_mut_slice())
        .map_err(|error| TransportError::io("generate mobile token file nonce", error))?;
    let temporary = home.join(format!(
        ".{TOKEN_FILE_NAME}.{}.tmp",
        URL_SAFE_NO_PAD.encode(nonce.as_slice())
    ));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true).mode(0o600);
    let mut file = options
        .open(&temporary)
        .map_err(|error| TransportError::io("open mobile token file", error))?;
    let write_result = (|| {
        let mut permissions = file
            .metadata()
            .map_err(|error| TransportError::io("inspect mobile token file", error))?
            .permissions();
        permissions.set_mode(0o600);
        file.set_permissions(permissions)
            .map_err(|error| TransportError::io("restrict mobile token file", error))?;
        file.write_all(token.as_bytes())
            .map_err(|error| TransportError::io("write mobile token file", error))?;
        file.sync_all()
            .map_err(|error| TransportError::io("sync mobile token file", error))?;
        std::fs::rename(&temporary, &path)
            .map_err(|error| TransportError::io("publish mobile token file", error))?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    write_result?;
    Ok(path)
}

#[cfg(not(unix))]
fn write_mobile_token_portable(path: PathBuf, token: &str) -> Result<PathBuf, TransportError> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&path)
        .map_err(|error| TransportError::io("open mobile token file", error))?;
    file.write_all(token.as_bytes())
        .map_err(|error| TransportError::io("write mobile token file", error))?;
    file.sync_all()
        .map_err(|error| TransportError::io("sync mobile token file", error))?;
    Ok(path)
}

fn mobile_apk_enabled() -> bool {
    std::env::var_os(MOBILE_APK_ENV).is_some_and(|value| value == OsStr::new("1"))
}

fn mobile_home(config: &DaemonConfig) -> PathBuf {
    std::env::var_os(MOBILE_HOME_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| config.store_dir.clone())
}

pub(crate) async fn start_if_enabled(
    config: &DaemonConfig,
    hub: crate::SessionHub,
    default_model: String,
    instance_id: String,
) -> Result<Option<MobileTransportServer>, DaemonError> {
    if !mobile_apk_enabled() {
        return Ok(None);
    }
    MobileTransportServer::start(
        &mobile_home(config),
        Some(MobileChatIntegration {
            hub,
            default_model,
            instance_id,
        }),
    )
    .await
    .map(Some)
    .map_err(|error| DaemonError::Task {
        message: format!("cannot start mobile APK transport: {error}"),
    })
}

pub(crate) struct MobileTransportServer {
    state: Arc<TransportState>,
    #[cfg(test)]
    address: SocketAddr,
    task: Option<JoinHandle<()>>,
}

struct MobileChatIntegration {
    hub: crate::SessionHub,
    default_model: String,
    instance_id: String,
}

impl MobileTransportServer {
    async fn start(
        home: &Path,
        integration: Option<MobileChatIntegration>,
    ) -> Result<Self, TransportError> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|error| TransportError::io("bind numeric mobile loopback listener", error))?;
        let address = listener
            .local_addr()
            .map_err(|error| TransportError::io("inspect mobile loopback listener", error))?;
        let SocketAddr::V4(address) = address else {
            return Err(TransportError::protocol(
                "mobile listener was not numeric IPv4 loopback",
            ));
        };
        if !address.ip().is_loopback() {
            return Err(TransportError::protocol(
                "mobile listener escaped the loopback interface",
            ));
        }

        let token = Arc::new(generate_token()?);
        let token_path = write_mobile_token(home, token.as_str())?;
        let state = Arc::new(TransportState::new());
        let mut server = Self {
            state,
            #[cfg(test)]
            address: SocketAddr::V4(address),
            task: None,
        };
        if let Some(integration) = integration {
            server.install_chat_bridge(
                integration.hub,
                integration.default_model,
                integration.instance_id,
            );
        }
        register_transport(&server.state);
        writeln!(
            std::io::stderr().lock(),
            "haiderd: mobile APK transport listening on {address}; token={} (written to {})",
            token.as_str(),
            token_path.display(),
        )
        .map_err(|error| TransportError::io("print mobile APK bootstrap", error))?;
        let accept_state = Arc::clone(&server.state);
        server.task = Some(tokio::spawn(async move {
            accept_connections(listener, accept_state, token).await;
        }));
        Ok(server)
    }

    pub(crate) async fn shutdown(&mut self) {
        self.state.clear_chat_bridge();
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
        self.state.shutdown_connection().await;
        clear_transport(&self.state);
    }

    #[cfg(test)]
    fn address(&self) -> SocketAddr {
        self.address
    }

    pub(crate) fn install_chat_bridge(
        &self,
        hub: crate::SessionHub,
        default_model: String,
        instance_id: String,
    ) {
        self.state
            .install_monitor_source_hub(hub.monitor_source_hub());
        let bridge = Arc::new(chat_bridge::DaemonMobileChatBridge::new(
            hub.clone(),
            default_model,
            instance_id,
            Arc::downgrade(&self.state),
        ));
        let monitor_sink = Arc::new(chat_bridge::MobileMonitorDeliverySink::new(Arc::downgrade(
            &bridge,
        )));
        self.state.install_chat_bridge(bridge.clone());
        hub.install_monitor_delivery_sink(monitor_sink);
    }
}

impl Drop for MobileTransportServer {
    fn drop(&mut self) {
        self.state.clear_chat_bridge();
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.state.shutdown_connection_now();
        clear_transport(&self.state);
    }
}

async fn accept_connections(
    listener: TcpListener,
    state: Arc<TransportState>,
    token: Arc<Zeroizing<String>>,
) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        tracing::error!(%error, "mobile APK listener failed");
                        break;
                    }
                };
                if connections.len() >= MAX_HANDSHAKES {
                    tracing::warn!(%peer, "mobile APK handshake capacity reached");
                    drop(stream);
                    continue;
                }
                let connection_state = Arc::clone(&state);
                let expected_token = Arc::clone(&token);
                connections.spawn(async move {
                    if let Err(error) = serve_connection(stream, connection_state, expected_token).await {
                        tracing::debug!(%peer, %error, "mobile APK connection closed");
                    }
                });
            }
            completed = connections.join_next(), if !connections.is_empty() => {
                if let Some(Err(error)) = completed {
                    tracing::debug!(%error, "mobile APK connection task ended unexpectedly");
                }
            }
        }
    }
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    state.shutdown_connection().await;
}

async fn serve_connection(
    mut stream: TcpStream,
    state: Arc<TransportState>,
    expected_token: Arc<Zeroizing<String>>,
) -> Result<(), TransportError> {
    stream
        .set_nodelay(true)
        .map_err(|error| TransportError::io("configure mobile TCP stream", error))?;
    let mut hello = match tokio::time::timeout(HANDSHAKE_TIMEOUT, read_frame(&mut stream)).await {
        Ok(Ok(hello)) => hello,
        Ok(Err(error)) => return Err(error),
        Err(_) => return Err(TransportError::protocol("mobile hello timed out")),
    };
    let hello_id = hello.id;
    let hello_result = validate_hello(&hello, expected_token.as_bytes());
    zeroize_hello_token(&mut hello);
    if let Err(reason) = hello_result {
        let rejection = Envelope {
            id: hello_id,
            body: json!({"type": "authReject", "reason": reason}),
        };
        write_frame(&mut stream, &rejection).await?;
        return Ok(());
    }
    let accepted = Envelope {
        id: hello_id,
        body: json!({"type": "authOk", "capabilities": SERVER_CAPABILITIES}),
    };
    write_frame(&mut stream, &accepted).await?;

    let connection_id = state.allocate_connection_id();
    let (commands, command_receiver) = mpsc::channel(CONNECTION_COMMAND_CAPACITY);
    let (monitor_streams, monitor_stream_receiver) = mpsc::channel(MONITOR_CHAT_STREAM_CAPACITY);
    let (close, close_receiver) = watch::channel(false);
    state
        .install_connection(Arc::new(ConnectionHandle {
            id: connection_id,
            commands,
            monitor_streams,
            close,
        }))
        .await;
    let result = connection_actor(
        stream,
        &state,
        connection_id,
        command_receiver,
        monitor_stream_receiver,
        close_receiver,
    )
    .await;
    state.clear_connection(connection_id).await;
    result
}

fn zeroize_hello_token(hello: &mut Envelope) {
    if let Some(Value::String(token)) = hello.body.get_mut("token") {
        token.zeroize();
    }
}

fn validate_hello(hello: &Envelope, expected_token: &[u8]) -> Result<(), &'static str> {
    let body = hello.body.as_object();
    let body_type = body
        .and_then(|body| body.get("type"))
        .and_then(Value::as_str);
    let token = body
        .and_then(|body| body.get("token"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let apk_version = body
        .and_then(|body| body.get("apkVersion"))
        .and_then(Value::as_str);
    let token_matches = constant_time_token_eq(expected_token, token.as_bytes());
    if hello.id != 1 || body_type != Some("hello") || apk_version.is_none_or(str::is_empty) {
        Err("invalid hello")
    } else if !token_matches {
        Err("invalid token")
    } else {
        Ok(())
    }
}

async fn connection_actor(
    stream: TcpStream,
    state: &Arc<TransportState>,
    connection_id: u64,
    mut commands: mpsc::Receiver<ActorCommand>,
    mut monitor_streams: mpsc::Receiver<MonitorChatStream>,
    mut close: watch::Receiver<bool>,
) -> Result<(), TransportError> {
    let (mut reader, mut writer) = stream.into_split();
    let mut pending = HashMap::<i64, oneshot::Sender<Result<Value, RequestFailure>>>::new();
    let (chat_output, mut chat_output_receiver) = mpsc::channel(CHAT_OUTPUT_CAPACITY);
    let (chat_commands, chat_command_receiver) = mpsc::channel(CHAT_COMMAND_CAPACITY);
    let (chat_worker_stop, chat_worker_stop_receiver) = watch::channel(false);
    let chat_worker = tokio::spawn(run_bridge_commands(
        chat_command_receiver,
        chat_worker_stop_receiver,
    ));
    let result = loop {
        tokio::select! {
            frame = read_frame(&mut reader) => {
                let frame = match frame {
                    Ok(frame) => frame,
                    Err(error) => break Err(error),
                };
                if !state.is_current(connection_id) {
                    break Ok(());
                }
                if is_bridge_request(&frame.body) {
                    if let Err(error) = dispatch_bridge_frame(
                        frame,
                        state,
                        &chat_output,
                        &chat_commands,
                    ).await {
                        break Err(error);
                    }
                    continue;
                }
                if let Err(error) = handle_inbound_frame(
                    frame,
                    state,
                    connection_id,
                    &mut pending,
                    &mut writer,
                ).await {
                    break Err(error);
                }
            }
            command = commands.recv() => {
                let Some(command) = command else {
                    break Ok(());
                };
                match command {
                    ActorCommand::Request {
                        body,
                        reply,
                        deadline,
                    } => {
                        pending.retain(|_, reply| !reply.is_closed());
                        if reply.is_closed() {
                            continue;
                        }
                        let _dispatch = match tokio::time::timeout_at(
                            deadline,
                            state.dispatch_gate.lock(),
                        ).await {
                            Ok(dispatch) => dispatch,
                            Err(_) => {
                                let _ = reply.send(Err(RequestFailure::TimedOut));
                                continue;
                            }
                        };
                        if reply.is_closed()
                            || tokio::time::Instant::now() >= deadline
                        {
                            let _ = reply.send(Err(RequestFailure::TimedOut));
                            continue;
                        }
                        if !state.is_current(connection_id) {
                            let _ = reply.send(Err(RequestFailure::Disconnected));
                            continue;
                        }
                        let id = state.allocate_request_id();
                        let envelope = Envelope { id, body };
                        match tokio::time::timeout_at(
                            deadline,
                            write_frame(&mut writer, &envelope),
                        ).await {
                            Ok(Ok(())) => {}
                            Ok(Err(error)) => {
                                let _ = reply.send(Err(RequestFailure::Disconnected));
                                break Err(error);
                            }
                            Err(_) => {
                                let _ = reply.send(Err(RequestFailure::TimedOut));
                                break Err(TransportError::protocol("mobile frame write timed out"));
                            }
                        }
                        pending.insert(id, reply);
                    }
                }
            }
            output = chat_output_receiver.recv() => {
                let Some(output) = output else {
                    break Err(TransportError::protocol("mobile chat output channel closed"));
                };
                let write = tokio::select! {
                    result = write_frame(&mut writer, &output) => Some(result),
                    changed = close.changed() => {
                        if changed.is_err() || *close.borrow() {
                            None
                        } else {
                            continue;
                        }
                    }
                };
                let Some(write) = write else {
                    break Ok(());
                };
                if let Err(error) = write {
                    break Err(error);
                }
            }
            stream = monitor_streams.recv() => {
                let Some(stream) = stream else {
                    break Err(TransportError::protocol("mobile monitor chat stream channel closed"));
                };
                if !state.is_current(connection_id) {
                    break Ok(());
                }
                let write = tokio::select! {
                    result = write_monitor_chat_stream(&mut writer, stream) => Some(result),
                    changed = close.changed() => {
                        if changed.is_err() || *close.borrow() {
                            None
                        } else {
                            continue;
                        }
                    }
                };
                let Some(write) = write else {
                    break Ok(());
                };
                if let Err(error) = write {
                    break Err(error);
                }
            }
            changed = close.changed() => {
                if changed.is_err() || *close.borrow() {
                    break Ok(());
                }
            }
        }
    };
    for (_, reply) in pending.drain() {
        let _ = reply.send(Err(RequestFailure::Disconnected));
    }
    chat_worker_stop.send_replace(true);
    // Do not abort an accepted canonical turn. The detached FIFO worker keeps
    // observing it until terminal or until its responder notices this actor's
    // dropped output receiver, at which point the bridge issues TurnCancel.
    drop(chat_worker);
    result
}

async fn write_monitor_chat_stream<W>(
    writer: &mut W,
    stream: MonitorChatStream,
) -> Result<(), TransportError>
where
    W: AsyncWrite + Unpin,
{
    let MonitorChatStream { id, text } = stream;
    for event in [
        ChatEvent::Delta {
            text,
            segment: "answer",
        },
        ChatEvent::Done,
    ] {
        let body = event
            .into_body()
            .map_err(|error| TransportError::protocol(error.to_string()))?;
        let envelope = Envelope { id, body };
        write_frame(writer, &envelope).await?;
    }
    Ok(())
}

fn is_bridge_request(body: &Value) -> bool {
    matches!(
        body.get("type").and_then(Value::as_str),
        Some("chat.send" | "session.config.get" | "session.select_model" | "session.select_effort")
    )
}

async fn dispatch_bridge_frame(
    frame: Envelope,
    state: &TransportState,
    output: &mpsc::Sender<Envelope>,
    commands: &mpsc::Sender<BridgeWork>,
) -> Result<(), TransportError> {
    if frame.id == 0 {
        return Err(TransportError::protocol(
            "mobile chat/session request used the reserved push id",
        ));
    }
    let request_is_chat = frame.body.get("type").and_then(Value::as_str) == Some("chat.send");
    let command = match ChatCommand::parse(&frame.body) {
        Ok(command) => command,
        Err(error) => {
            try_enqueue_bridge_event(output, frame.id, error.into_event(request_is_chat))?;
            return Ok(());
        }
    };
    let Some(bridge) = state.chat_bridge() else {
        try_enqueue_bridge_event(
            output,
            frame.id,
            MobileChatError::new(
                "daemon_initializing",
                "The daemon chat session is not ready yet",
                true,
            )
            .into_event(command.is_chat()),
        )?;
        return Ok(());
    };
    let responder = ChatResponder {
        id: frame.id,
        frames: output.clone(),
    };
    let is_chat = command.is_chat();
    match commands.try_send(BridgeWork {
        bridge,
        command,
        responder,
        is_chat,
    }) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(work)) => {
            try_enqueue_bridge_event(
                output,
                work.responder.id,
                MobileChatError::new(
                    "bridge_busy",
                    "Too many mobile chat requests are already queued",
                    true,
                )
                .into_event(work.is_chat),
            )?;
        }
        Err(mpsc::error::TrySendError::Closed(work)) => {
            try_enqueue_bridge_event(
                output,
                work.responder.id,
                MobileChatError::internal("mobile chat worker is unavailable")
                    .into_event(work.is_chat),
            )?;
        }
    }
    Ok(())
}

struct BridgeWork {
    bridge: Arc<dyn MobileChatBridge>,
    command: ChatCommand,
    responder: ChatResponder,
    is_chat: bool,
}

async fn run_bridge_commands(
    mut commands: mpsc::Receiver<BridgeWork>,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        if *stop.borrow() {
            return;
        }
        let work = tokio::select! {
            work = commands.recv() => work,
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    return;
                }
                continue;
            }
        };
        let Some(work) = work else { return };
        let error_responder = work.responder.clone();
        if let Err(error) = work.bridge.handle(work.command, work.responder).await {
            let _ = error_responder.send(error.into_event(work.is_chat)).await;
        }
    }
}

fn try_enqueue_bridge_event(
    output: &mpsc::Sender<Envelope>,
    id: i64,
    event: ChatEvent,
) -> Result<(), TransportError> {
    let body = event
        .into_body()
        .map_err(|error| TransportError::protocol(error.to_string()))?;
    output
        .try_send(Envelope { id, body })
        .map_err(|_| TransportError::protocol("mobile chat output queue is full"))
}

async fn handle_inbound_frame<W>(
    frame: Envelope,
    state: &Arc<TransportState>,
    connection_id: u64,
    pending: &mut HashMap<i64, oneshot::Sender<Result<Value, RequestFailure>>>,
    writer: &mut W,
) -> Result<(), TransportError>
where
    W: AsyncWrite + Unpin,
{
    let frame_type = frame
        .body
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| TransportError::protocol("mobile frame body has no string type"))?
        .to_owned();
    if frame.id == 0 {
        return route_push(state, connection_id, frame.body).await;
    }
    if let Some(reply) = pending.remove(&frame.id) {
        let _ = reply.send(Ok(frame.body));
    } else {
        tracing::debug!(id = frame.id, %frame_type, "ignoring unmatched mobile response");
    }
    Ok(())
}

async fn route_push(
    state: &TransportState,
    connection_id: u64,
    body: Value,
) -> Result<(), TransportError> {
    let _dispatch = state.dispatch_gate.lock().await;
    if !state.is_current(connection_id) {
        return Ok(());
    }
    let push_type = body.get("type").and_then(Value::as_str).map(str::to_owned);
    match push_type.as_deref() {
        Some("sms.incoming") => {
            let push: IncomingSmsPush = serde_json::from_value(body).map_err(|error| {
                TransportError::protocol(format!("invalid sms.incoming push: {error}"))
            })?;
            if push.address.len() > MAX_SMS_PUSH_ADDRESS_BYTES
                || push.body.len() > MAX_SMS_PUSH_BODY_BYTES
                || push.ts < 0
            {
                return Err(TransportError::protocol(
                    "sms.incoming push exceeds a field limit or has a negative timestamp",
                ));
            }
            let monitor_source = state.monitor_source_hub().ok_or_else(|| {
                TransportError::protocol("mobile monitor source is not installed")
            })?;
            publish_monitor_sms(&monitor_source, &push);
            state.record_incoming_sms(push.clone());
            let _ = state.incoming_sms.send(push);
            Ok(())
        }
        Some("capabilities.changed") => {
            let push: CapabilitiesChangedPush = serde_json::from_value(body).map_err(|error| {
                TransportError::protocol(format!("invalid capabilities.changed push: {error}"))
            })?;
            if push.granted.len() > MAX_GRANTED_CAPABILITIES
                || push
                    .granted
                    .iter()
                    .any(|capability| capability.len() > MAX_CAPABILITY_NAME_BYTES)
            {
                return Err(TransportError::protocol(
                    "capabilities.changed push exceeds the field-size limit",
                ));
            }
            state.set_capabilities(push.granted);
            Ok(())
        }
        Some(other) => Err(TransportError::protocol(format!(
            "unsupported mobile push type `{other}`"
        ))),
        None => Err(TransportError::protocol(
            "mobile push body has no string type",
        )),
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct IncomingSmsPush {
    #[serde(rename = "type")]
    _push_type: String,
    address: String,
    body: String,
    ts: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CapabilitiesChangedPush {
    #[serde(rename = "type")]
    _push_type: String,
    granted: Vec<String>,
}

enum ActorCommand {
    Request {
        body: Value,
        reply: oneshot::Sender<Result<Value, RequestFailure>>,
        deadline: tokio::time::Instant,
    },
}

#[derive(Debug, Clone, Copy)]
enum RequestFailure {
    Disconnected,
    TimedOut,
}

struct ConnectionHandle {
    id: u64,
    commands: mpsc::Sender<ActorCommand>,
    monitor_streams: mpsc::Sender<MonitorChatStream>,
    close: watch::Sender<bool>,
}

struct TransportState {
    connection: StdRwLock<Option<Arc<ConnectionHandle>>>,
    connected: AtomicBool,
    next_connection_id: AtomicU64,
    next_request_id: AtomicI64,
    capabilities: StdRwLock<Vec<String>>,
    capabilities_seen: AtomicBool,
    incoming_sms: broadcast::Sender<IncomingSmsPush>,
    recent_sms: StdMutex<RecentSmsCache>,
    dispatch_gate: AsyncMutex<()>,
    chat_bridge: StdRwLock<Option<Arc<dyn MobileChatBridge>>>,
    monitor_source_hub: StdRwLock<Option<MonitorSourceHub>>,
}

#[derive(Default)]
struct RecentSmsCache {
    entries: VecDeque<IncomingSmsPush>,
    bytes: usize,
}

impl RecentSmsCache {
    fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl TransportState {
    fn new() -> Self {
        let (incoming_sms, _) = broadcast::channel(64);
        Self {
            connection: StdRwLock::new(None),
            connected: AtomicBool::new(false),
            next_connection_id: AtomicU64::new(1),
            // Negative IDs cannot collide with the APK's positive chat IDs.
            next_request_id: AtomicI64::new(-1),
            capabilities: StdRwLock::new(Vec::new()),
            capabilities_seen: AtomicBool::new(false),
            incoming_sms,
            recent_sms: StdMutex::new(RecentSmsCache {
                entries: VecDeque::with_capacity(RECENT_SMS_CAPACITY),
                bytes: 0,
            }),
            dispatch_gate: AsyncMutex::new(()),
            chat_bridge: StdRwLock::new(None),
            monitor_source_hub: StdRwLock::new(None),
        }
    }

    fn allocate_connection_id(&self) -> u64 {
        self.next_connection_id.fetch_add(1, Ordering::Relaxed)
    }

    fn allocate_request_id(&self) -> i64 {
        self.next_request_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| {
                Some(if id == i64::MIN { -1 } else { id - 1 })
            })
            .unwrap_or(-1)
    }

    async fn install_connection(&self, connection: Arc<ConnectionHandle>) {
        // Linearize replacement against request writes: once a new APK wins
        // this gate, no queued request can escape to the displaced socket.
        let _dispatch = self.dispatch_gate.lock().await;
        let prior = write_lock(&self.connection).replace(connection);
        write_lock(&self.capabilities).clear();
        mutex_lock(&self.recent_sms).clear();
        self.capabilities_seen.store(false, Ordering::Release);
        self.connected.store(true, Ordering::Release);
        if let Some(prior) = prior {
            prior.close.send_replace(true);
        }
    }

    fn current_connection(&self) -> Option<Arc<ConnectionHandle>> {
        read_lock(&self.connection).clone()
    }

    fn is_current(&self, connection_id: u64) -> bool {
        read_lock(&self.connection)
            .as_ref()
            .is_some_and(|connection| connection.id == connection_id)
    }

    async fn clear_connection(&self, connection_id: u64) {
        let _dispatch = self.dispatch_gate.lock().await;
        let mut active = write_lock(&self.connection);
        if active
            .as_ref()
            .is_some_and(|connection| connection.id == connection_id)
        {
            active.take();
            self.connected.store(false, Ordering::Release);
            write_lock(&self.capabilities).clear();
            mutex_lock(&self.recent_sms).clear();
            self.capabilities_seen.store(false, Ordering::Release);
        }
    }

    async fn shutdown_connection(&self) {
        let _dispatch = self.dispatch_gate.lock().await;
        self.shutdown_connection_now();
    }

    fn shutdown_connection_now(&self) {
        let active = write_lock(&self.connection).take();
        self.connected.store(false, Ordering::Release);
        write_lock(&self.capabilities).clear();
        mutex_lock(&self.recent_sms).clear();
        self.capabilities_seen.store(false, Ordering::Release);
        if let Some(active) = active {
            active.close.send_replace(true);
        }
    }

    fn set_capabilities(&self, mut capabilities: Vec<String>) {
        capabilities.sort_unstable();
        capabilities.dedup();
        *write_lock(&self.capabilities) = capabilities;
        self.capabilities_seen.store(true, Ordering::Release);
    }

    fn capability_granted(&self, capability: &str) -> bool {
        read_lock(&self.capabilities)
            .binary_search_by(|granted| granted.as_str().cmp(capability))
            .is_ok()
    }

    fn record_incoming_sms(&self, push: IncomingSmsPush) {
        let mut recent = mutex_lock(&self.recent_sms);
        let push_bytes = push.address.len().saturating_add(push.body.len());
        while !recent.entries.is_empty()
            && (recent.entries.len() == RECENT_SMS_CAPACITY
                || recent.bytes.saturating_add(push_bytes) > RECENT_SMS_MAX_BYTES)
        {
            if let Some(removed) = recent.entries.pop_front() {
                recent.bytes = recent
                    .bytes
                    .saturating_sub(removed.address.len().saturating_add(removed.body.len()));
            }
        }
        recent.bytes = recent.bytes.saturating_add(push_bytes);
        recent.entries.push_back(push);
    }

    fn recent_sms(&self) -> Vec<IncomingSmsPush> {
        mutex_lock(&self.recent_sms)
            .entries
            .iter()
            .cloned()
            .collect()
    }

    fn install_chat_bridge(&self, bridge: Arc<dyn MobileChatBridge>) {
        *write_lock(&self.chat_bridge) = Some(bridge);
    }

    fn install_monitor_source_hub(&self, hub: MonitorSourceHub) {
        *write_lock(&self.monitor_source_hub) = Some(hub);
    }

    fn monitor_source_hub(&self) -> Option<MonitorSourceHub> {
        read_lock(&self.monitor_source_hub).clone()
    }

    async fn send_monitor_chat(&self, text: String) -> Result<i64, MobileChatError> {
        let connection = self.current_connection().ok_or_else(|| {
            MobileChatError::new(
                "mobile_chat_unavailable",
                "the mobile chat transport is not connected",
                true,
            )
        })?;
        if !self.is_current(connection.id) {
            return Err(MobileChatError::new(
                "mobile_chat_unavailable",
                "the mobile chat transport changed before monitor delivery",
                true,
            ));
        }
        let id = self.allocate_request_id();
        connection
            .monitor_streams
            .send(MonitorChatStream { id, text })
            .await
            .map_err(|_| {
                MobileChatError::new(
                    "mobile_chat_unavailable",
                    "the mobile monitor chat stream is closed",
                    true,
                )
            })?;
        Ok(id)
    }

    fn chat_bridge(&self) -> Option<Arc<dyn MobileChatBridge>> {
        read_lock(&self.chat_bridge).clone()
    }

    fn clear_chat_bridge(&self) {
        write_lock(&self.chat_bridge).take();
    }
}

fn publish_monitor_sms(hub: &MonitorSourceHub, push: &IncomingSmsPush) {
    match publish_sms_incoming(hub, &push.address, &push.body, push.ts) {
        Ok(receipt) if receipt.saturated_subscribers > 0 => {
            tracing::warn!(
                sequence = receipt.sequence,
                saturated = receipt.saturated_subscribers,
                "monitor SMS source queue is saturated"
            );
        }
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(%error, "validated SMS push could not reach monitor source hub");
        }
    }
}

fn read_lock<T>(lock: &StdRwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write_lock<T>(lock: &StdRwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn mutex_lock<T>(lock: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn transport_registry() -> &'static StdRwLock<Weak<TransportState>> {
    static REGISTRY: OnceLock<StdRwLock<Weak<TransportState>>> = OnceLock::new();
    REGISTRY.get_or_init(|| StdRwLock::new(Weak::new()))
}

fn register_transport(state: &Arc<TransportState>) {
    *write_lock(transport_registry()) = Arc::downgrade(state);
}

fn clear_transport(state: &Arc<TransportState>) {
    let mut registered = write_lock(transport_registry());
    if registered
        .upgrade()
        .is_some_and(|active| Arc::ptr_eq(&active, state))
    {
        *registered = Weak::new();
    }
}

pub(crate) fn platform_mobile_backend() -> Arc<dyn MobileBackend> {
    let state = read_lock(transport_registry()).upgrade();
    if let Some(state) = state
        && state.connected.load(Ordering::Acquire)
    {
        return Arc::new(ApkMobileBackend { state });
    }
    haider_tools::platform_mobile_backend()
}

struct ApkMobileBackend {
    state: Arc<TransportState>,
}

impl ApkMobileBackend {
    async fn request(&self, body: Value) -> MobileResult<Value> {
        let connection = self
            .state
            .current_connection()
            .ok_or_else(apk_disconnected)?;
        let (reply, response) = oneshot::channel();
        let deadline = tokio::time::Instant::now() + BACKEND_REQUEST_TIMEOUT;
        let exchange = async {
            connection
                .commands
                .send(ActorCommand::Request {
                    body,
                    reply,
                    deadline,
                })
                .await
                .map_err(|_| RequestFailure::Disconnected)?;
            response.await.map_err(|_| RequestFailure::Disconnected)?
        };
        match tokio::time::timeout_at(deadline, exchange).await {
            Ok(Ok(body)) => Ok(body),
            Ok(Err(RequestFailure::Disconnected)) => Err(apk_disconnected()),
            Ok(Err(RequestFailure::TimedOut)) | Err(_) => Err(MobileError::Backend {
                message: "mobile APK request timed out".into(),
            }),
        }
    }

    async fn snapshot(&self, cancel: &MobileCancelToken) -> MobileResult<Vec<A11yNode>> {
        cancel.check()?;
        let response = self.request(json!({"type": "a11y.snapshot"})).await?;
        cancel.check()?;
        translate_a11y_tree(response)
    }

    async fn coordinates_for(
        &self,
        element_id: &Option<String>,
        x: Option<i32>,
        y: Option<i32>,
        cancel: &MobileCancelToken,
    ) -> MobileResult<(i32, i32)> {
        if let (Some(x), Some(y)) = (x, y) {
            return Ok((x, y));
        }
        let element_id = element_id
            .as_deref()
            .ok_or_else(|| MobileError::InvalidAction {
                message: "mobile target has neither coordinates nor an element id".into(),
            })?;
        let node = self
            .snapshot(cancel)
            .await?
            .into_iter()
            .find(|node| node.id == element_id)
            .ok_or_else(|| MobileError::InvalidAction {
                message: format!(
                    "mobile accessibility element `{element_id}` is no longer present"
                ),
            })?;
        Ok((
            midpoint(node.bounds.left, node.bounds.right),
            midpoint(node.bounds.top, node.bounds.bottom),
        ))
    }

    fn require_granted_capability(&self, action: &MobileAction) -> MobileResult<()> {
        let required = match action {
            MobileAction::Screenshot {} => Some("screenCapture"),
            MobileAction::A11yTree {}
            | MobileAction::Inspect { .. }
            | MobileAction::Tap { .. }
            | MobileAction::LongPress { .. }
            | MobileAction::Swipe { .. }
            | MobileAction::Type { .. }
            | MobileAction::Key { .. } => Some("accessibility"),
            MobileAction::SmsRead { .. } => Some("smsRead"),
            MobileAction::OpenApp { .. } | MobileAction::ListApps {} => None,
        };
        if let Some(required) = required {
            if !self.state.capabilities_seen.load(Ordering::Acquire) {
                return Err(MobileError::Unavailable {
                    message: "mobile APK has not reported its granted capabilities yet".into(),
                });
            }
            if !self.state.capability_granted(required) {
                return Err(MobileError::Unavailable {
                    message: format!("mobile APK capability `{required}` is not granted"),
                });
            }
        }
        Ok(())
    }

    fn merge_recent_sms(
        &self,
        mut messages: Vec<SmsMessage>,
        since_ms: Option<i64>,
        limit: Option<i32>,
    ) -> Vec<SmsMessage> {
        for (index, pushed) in self.state.recent_sms().into_iter().enumerate() {
            if since_ms.is_some_and(|since| pushed.ts < since)
                || messages.iter().any(|message| {
                    message.date_ms == pushed.ts
                        && message.address == pushed.address
                        && message.body == pushed.body
                })
            {
                continue;
            }
            messages.push(SmsMessage {
                id: format!("apk-push-{}-{index}", pushed.ts),
                address: pushed.address,
                body: pushed.body,
                date_ms: pushed.ts,
                folder: "inbox".into(),
            });
        }
        messages.sort_by_key(|message| std::cmp::Reverse(message.date_ms));
        if let Ok(limit) = usize::try_from(limit.unwrap_or(DEFAULT_SMS_LIMIT)) {
            messages.truncate(limit);
        }
        messages
    }
}

#[async_trait]
impl MobileBackend for ApkMobileBackend {
    async fn prepare(&self, action: &MobileAction, cancel: &MobileCancelToken) -> MobileResult<()> {
        cancel.check()?;
        if self.state.connected.load(Ordering::Acquire) {
            self.require_granted_capability(action)
        } else {
            Err(apk_disconnected())
        }
    }

    async fn execute(
        &self,
        action: &MobileAction,
        cancel: &MobileCancelToken,
    ) -> MobileResult<MobileOutput> {
        cancel.check()?;
        if !self.state.connected.load(Ordering::Acquire) {
            return Err(apk_disconnected());
        }
        self.require_granted_capability(action)?;
        match action {
            MobileAction::Screenshot {} => {
                let response = self.request(json!({"type": "screen.capture"})).await?;
                cancel.check()?;
                translate_png(response).map(MobileOutput::Screenshot)
            }
            MobileAction::A11yTree {} => self.snapshot(cancel).await.map(MobileOutput::A11yTree),
            MobileAction::Inspect { element_id, x, y } => {
                let mut nodes = self.snapshot(cancel).await?;
                if let Some(element_id) = element_id {
                    nodes.retain(|node| &node.id == element_id);
                } else if let (Some(x), Some(y)) = (x, y) {
                    nodes.retain(|node| point_inside(node.bounds, *x, *y));
                }
                Ok(MobileOutput::A11yTree(nodes))
            }
            MobileAction::Tap { element_id, x, y } => {
                let (x, y) = self.coordinates_for(element_id, *x, *y, cancel).await?;
                let response = self
                    .request(json!({
                        "type": "a11y.tap",
                        "x": x,
                        "y": y,
                        "control": is_control(action),
                    }))
                    .await?;
                cancel.check()?;
                translate_ack(response)
            }
            MobileAction::LongPress { .. } => Err(MobileError::InvalidAction {
                message: "the connected APK does not support long_press".into(),
            }),
            MobileAction::Swipe { from, to } => {
                let response = self
                    .request(json!({
                        "type": "a11y.swipe",
                        "x1": from.x,
                        "y1": from.y,
                        "x2": to.x,
                        "y2": to.y,
                        "ms": DEFAULT_SWIPE_MS,
                        "control": is_control(action),
                    }))
                    .await?;
                cancel.check()?;
                translate_ack(response)
            }
            MobileAction::Type { text } => {
                let response = self
                    .request(json!({
                        "type": "a11y.text",
                        "text": text,
                        "control": is_control(action),
                    }))
                    .await?;
                cancel.check()?;
                translate_ack(response)
            }
            MobileAction::Key { .. } => Err(MobileError::InvalidAction {
                message: "the connected APK does not support mobile key events".into(),
            }),
            MobileAction::OpenApp { package, .. } => {
                let package = package
                    .as_deref()
                    .ok_or_else(|| MobileError::InvalidAction {
                        message: "the connected APK requires open_app.package".into(),
                    })?;
                let response = self
                    .request(json!({
                        "type": "app.open",
                        "pkg": package,
                        "control": is_control(action),
                    }))
                    .await?;
                cancel.check()?;
                translate_ack(response)
            }
            MobileAction::ListApps {} => Err(MobileError::InvalidAction {
                message: "the connected APK does not support listing installed apps".into(),
            }),
            MobileAction::SmsRead {
                folder,
                since,
                limit,
            } => {
                if folder
                    .as_deref()
                    .is_some_and(|folder| !folder.eq_ignore_ascii_case("inbox"))
                {
                    return Err(MobileError::InvalidAction {
                        message: "the connected APK supports only the inbox SMS folder".into(),
                    });
                }
                let since_ms = since
                    .as_deref()
                    .map(str::parse::<i64>)
                    .transpose()
                    .map_err(|_| MobileError::InvalidAction {
                        message: "the connected APK requires sms_read.since as Unix milliseconds"
                            .into(),
                    })?;
                let limit = limit.map(i32::try_from).transpose().map_err(|_| {
                    MobileError::InvalidAction {
                        message: "sms_read.limit exceeds the APK integer range".into(),
                    }
                })?;
                let response = self
                    .request(json!({
                        "type": "sms.list",
                        "sinceMs": since_ms,
                        "limit": limit,
                    }))
                    .await?;
                cancel.check()?;
                let messages = translate_sms_list(response)?;
                Ok(MobileOutput::SmsList(
                    self.merge_recent_sms(messages, since_ms, limit),
                ))
            }
        }
    }
}

fn apk_disconnected() -> MobileError {
    MobileError::Unavailable {
        message: "mobile APK is not connected".into(),
    }
}

fn is_control(action: &MobileAction) -> bool {
    action.effect_class() == EffectClass::MobileControl
}

fn midpoint(first: i32, second: i32) -> i32 {
    i32::try_from((i64::from(first) + i64::from(second)) / 2).unwrap_or(first)
}

fn point_inside(bounds: Point4, x: i32, y: i32) -> bool {
    x >= bounds.left && x <= bounds.right && y >= bounds.top && y <= bounds.bottom
}

fn response_type(response: &Value) -> MobileResult<&str> {
    let response_type = response
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| MobileError::Backend {
            message: "mobile APK response has no string type".into(),
        })?;
    match response_type {
        "rejected" | "error" => {
            let reason = response
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("unspecified APK failure");
            Err(MobileError::Backend {
                message: format!("mobile APK rejected the request: {reason}"),
            })
        }
        _ => Ok(response_type),
    }
}

fn translate_ack(response: Value) -> MobileResult<MobileOutput> {
    if response_type(&response)? != "ack" {
        return Err(unexpected_response("ack", &response));
    }
    match response.get("ok").and_then(Value::as_bool) {
        Some(true) => Ok(MobileOutput::Ack),
        Some(false) => Err(MobileError::Backend {
            message: "mobile APK could not perform the requested action".into(),
        }),
        None => Err(MobileError::Backend {
            message: "mobile APK ack has no boolean ok field".into(),
        }),
    }
}

fn translate_png(response: Value) -> MobileResult<Vec<u8>> {
    if response_type(&response)? != "png" {
        return Err(unexpected_response("png", &response));
    }
    let encoded = response
        .get("base64")
        .and_then(Value::as_str)
        .ok_or_else(|| MobileError::Backend {
            message: "mobile APK png response has no base64 field".into(),
        })?;
    STANDARD.decode(encoded).map_err(|_| MobileError::Backend {
        message: "mobile APK returned invalid PNG base64".into(),
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireA11yNode {
    #[serde(default)]
    text: String,
    #[serde(default)]
    content_desc: String,
    #[serde(default)]
    class_name: String,
    #[serde(default)]
    resource_id: String,
    bounds: [i32; 4],
}

#[derive(Debug, Deserialize)]
struct WireA11yTree {
    nodes: Vec<WireA11yNode>,
}

fn translate_a11y_tree(response: Value) -> MobileResult<Vec<A11yNode>> {
    if response_type(&response)? != "a11yTree" {
        return Err(unexpected_response("a11yTree", &response));
    }
    let tree: WireA11yTree =
        serde_json::from_value(response).map_err(|error| MobileError::Backend {
            message: format!("mobile APK returned an invalid accessibility tree: {error}"),
        })?;
    Ok(tree
        .nodes
        .into_iter()
        .enumerate()
        .map(|(index, node)| {
            let resource_id = nonempty(node.resource_id);
            A11yNode {
                id: resource_id
                    .as_deref()
                    .map_or_else(|| format!("node-{index}"), |id| format!("{id}#{index}")),
                text: nonempty(node.text),
                content_desc: nonempty(node.content_desc),
                class: node.class_name,
                resource_id,
                bounds: Point4 {
                    left: node.bounds[0],
                    top: node.bounds[1],
                    right: node.bounds[2],
                    bottom: node.bounds[3],
                },
            }
        })
        .collect())
}

fn nonempty(value: String) -> Option<String> {
    if value.is_empty() { None } else { Some(value) }
}

#[derive(Debug, Deserialize)]
struct WireSmsMessage {
    address: String,
    body: String,
    ts: i64,
    #[serde(default)]
    read: bool,
}

#[derive(Debug, Deserialize)]
struct WireSmsList {
    messages: Vec<WireSmsMessage>,
}

fn translate_sms_list(response: Value) -> MobileResult<Vec<SmsMessage>> {
    if response_type(&response)? != "smsList" {
        return Err(unexpected_response("smsList", &response));
    }
    let list: WireSmsList =
        serde_json::from_value(response).map_err(|error| MobileError::Backend {
            message: format!("mobile APK returned an invalid SMS list: {error}"),
        })?;
    Ok(list
        .messages
        .into_iter()
        .enumerate()
        .map(|(index, message)| {
            let _ = message.read;
            SmsMessage {
                id: format!("apk-sms-{}-{index}", message.ts),
                address: message.address,
                body: message.body,
                date_ms: message.ts,
                folder: "inbox".into(),
            }
        })
        .collect())
}

fn unexpected_response(expected: &str, response: &Value) -> MobileError {
    let actual = response
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("missing");
    MobileError::Backend {
        message: format!("mobile APK returned `{actual}` where `{expected}` was required"),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use haider_protocol::mobile::Point;

    #[tokio::test]
    async fn frame_codec_round_trips_big_endian_json_without_a_socket() {
        let expected = Envelope {
            id: 42,
            body: json!({"type": "ack", "ok": true}),
        };
        let encoded = encode_frame(&expected).expect("encode frame");
        assert_eq!(
            u32::from_be_bytes(encoded[..4].try_into().expect("prefix")) as usize,
            encoded.len() - 4,
        );
        let (mut writer, mut reader) = tokio::io::duplex(1024);
        writer.write_all(&encoded).await.expect("write duplex");
        assert_eq!(read_frame(&mut reader).await.expect("read frame"), expected);
    }

    #[tokio::test]
    async fn frame_codec_rejects_oversized_length_before_allocating_payload() {
        let (mut writer, mut reader) = tokio::io::duplex(16);
        writer
            .write_all(&((MAX_FRAME_BYTES as u32) + 1).to_be_bytes())
            .await
            .expect("write prefix");
        let error = read_frame(&mut reader).await.expect_err("oversized frame");
        assert!(error.to_string().contains("8 MiB"));
    }

    #[test]
    fn token_comparison_accepts_only_the_complete_exact_token() {
        let token = b"abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG";
        assert!(constant_time_token_eq(token, token));
        assert!(!constant_time_token_eq(
            token,
            b"abcdefghijklmnopqrstuvwxyz0123456789ABCDEFH"
        ));
        assert!(!constant_time_token_eq(token, &token[..token.len() - 1]));
        let mut longer = token.to_vec();
        longer.push(b'x');
        assert!(!constant_time_token_eq(token, &longer));
    }

    #[test]
    fn hello_validation_rejects_wrong_tokens_and_wrong_ids() {
        let hello = Envelope {
            id: 1,
            body: json!({"type": "hello", "token": "wrong", "apkVersion": "test"}),
        };
        assert_eq!(validate_hello(&hello, b"right"), Err("invalid token"));
        let wrong_id = Envelope {
            id: 2,
            body: json!({"type": "hello", "token": "right", "apkVersion": "test"}),
        };
        assert_eq!(validate_hello(&wrong_id, b"right"), Err("invalid hello"));
    }

    #[test]
    fn token_file_is_written_without_a_secret_copy_in_the_error_path() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = write_mobile_token(directory.path(), "token-sentinel").expect("write token");
        assert_eq!(
            std::fs::read_to_string(path).expect("read token"),
            "token-sentinel"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(directory.path().join(TOKEN_FILE_NAME))
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn apk_accessibility_and_sms_shapes_translate_to_protocol_types() {
        let nodes = translate_a11y_tree(json!({
            "type": "a11yTree",
            "nodes": [{
                "text": "Send",
                "contentDesc": "Send message",
                "className": "android.widget.Button",
                "resourceId": "com.example:id/send",
                "bounds": [10, 20, 30, 40],
                "clickable": true
            }]
        }))
        .expect("a11y tree");
        assert_eq!(nodes[0].id, "com.example:id/send#0");
        assert_eq!(nodes[0].bounds.right, 30);

        let messages = translate_sms_list(json!({
            "type": "smsList",
            "messages": [{"address": "+1555", "body": "hello", "ts": 123, "read": true}]
        }))
        .expect("sms list");
        assert_eq!(messages[0].id, "apk-sms-123-0");
        assert_eq!(messages[0].folder, "inbox");
    }

    #[tokio::test]
    async fn apk_pushes_reach_their_typed_routes() {
        let state = TransportState::new();
        let (commands, _receiver) = mpsc::channel(CONNECTION_COMMAND_CAPACITY);
        let (monitor_streams, mut monitor_stream_receiver) =
            mpsc::channel(MONITOR_CHAT_STREAM_CAPACITY);
        let (close, _close_receiver) = watch::channel(false);
        state
            .install_connection(Arc::new(ConnectionHandle {
                id: 9,
                commands,
                monitor_streams,
                close,
            }))
            .await;
        let monitor_sources = MonitorSourceHub::new();
        let mut monitor_sms = monitor_sources.subscribe(haider_tools::MonitorSourceKind::Sms);
        state.install_monitor_source_hub(monitor_sources);
        let mut incoming_sms = state.incoming_sms.subscribe();
        route_push(
            &state,
            9,
            json!({
                "type": "sms.incoming",
                "address": "+1555",
                "body": "new message",
                "ts": 456
            }),
        )
        .await
        .expect("route SMS push");
        assert_eq!(
            incoming_sms.recv().await.expect("incoming SMS"),
            IncomingSmsPush {
                _push_type: "sms.incoming".into(),
                address: "+1555".into(),
                body: "new message".into(),
                ts: 456,
            }
        );
        let monitor_event = monitor_sms.recv().await.expect("monitor SMS source event");
        assert!(matches!(
            monitor_event.payload,
            crate::MonitorEventPayload::Sms(crate::SmsIncomingEvent {
                address,
                body,
                received_at_ms: 456,
            }) if address == "+1555" && body == "new message"
        ));
        let monitor_chat_id = state
            .send_monitor_chat("monitor report".into())
            .await
            .expect("queue monitor chat stream");
        assert!(monitor_chat_id < 0);
        assert_eq!(
            monitor_stream_receiver
                .recv()
                .await
                .expect("monitor chat stream"),
            MonitorChatStream {
                id: monitor_chat_id,
                text: "monitor report".into(),
            }
        );
        route_push(
            &state,
            9,
            json!({"type": "capabilities.changed", "granted": ["smsRead", "accessibility", "smsRead"]}),
        )
        .await
        .expect("route capability push");
        assert_eq!(
            read_lock(&state.capabilities).as_slice(),
            ["accessibility", "smsRead"]
        );
        assert!(state.capabilities_seen.load(Ordering::Acquire));
        assert_eq!(mutex_lock(&state.recent_sms).len(), 1);
    }

    #[tokio::test]
    async fn last_authenticated_connection_closes_prior_and_resets_push_state() {
        let state = TransportState::new();
        let (first_commands, _first_receiver) = mpsc::channel(CONNECTION_COMMAND_CAPACITY);
        let (first_monitor_streams, _first_monitor_stream_receiver) =
            mpsc::channel(MONITOR_CHAT_STREAM_CAPACITY);
        let (first_close, first_close_receiver) = watch::channel(false);
        state
            .install_connection(Arc::new(ConnectionHandle {
                id: 1,
                commands: first_commands,
                monitor_streams: first_monitor_streams,
                close: first_close,
            }))
            .await;
        state.set_capabilities(vec!["smsRead".into()]);
        state.record_incoming_sms(IncomingSmsPush {
            _push_type: "sms.incoming".into(),
            address: "+1555".into(),
            body: "old".into(),
            ts: 1,
        });

        let (second_commands, _second_receiver) = mpsc::channel(CONNECTION_COMMAND_CAPACITY);
        let (second_monitor_streams, _second_monitor_stream_receiver) =
            mpsc::channel(MONITOR_CHAT_STREAM_CAPACITY);
        let (second_close, _second_close_receiver) = watch::channel(false);
        state
            .install_connection(Arc::new(ConnectionHandle {
                id: 2,
                commands: second_commands,
                monitor_streams: second_monitor_streams,
                close: second_close,
            }))
            .await;
        assert!(*first_close_receiver.borrow());
        assert!(state.is_current(2));
        assert!(!state.capabilities_seen.load(Ordering::Acquire));
        assert!(read_lock(&state.capabilities).is_empty());
        assert!(mutex_lock(&state.recent_sms).is_empty());
    }

    async fn connected_backend() -> (
        ApkMobileBackend,
        mpsc::Receiver<ActorCommand>,
        watch::Receiver<bool>,
    ) {
        let state = Arc::new(TransportState::new());
        let (commands, receiver) = mpsc::channel(CONNECTION_COMMAND_CAPACITY);
        let (monitor_streams, _monitor_stream_receiver) =
            mpsc::channel(MONITOR_CHAT_STREAM_CAPACITY);
        let (close, close_receiver) = watch::channel(false);
        state
            .install_connection(Arc::new(ConnectionHandle {
                id: 7,
                commands,
                monitor_streams,
                close,
            }))
            .await;
        state.set_capabilities(vec![
            "accessibility".into(),
            "screenCapture".into(),
            "smsRead".into(),
        ]);
        (ApkMobileBackend { state }, receiver, close_receiver)
    }

    #[tokio::test]
    async fn backend_translates_screen_capture_without_a_socket() {
        let (backend, mut commands, _close) = connected_backend().await;
        let responder = tokio::spawn(async move {
            let ActorCommand::Request { body, reply, .. } =
                commands.recv().await.expect("capability request");
            assert_eq!(body, json!({"type": "screen.capture"}));
            reply
                .send(Ok(json!({
                    "type": "png",
                    "base64": STANDARD.encode(b"png-bytes")
                })))
                .expect("reply receiver");
        });
        let output = backend
            .execute(&MobileAction::Screenshot {}, &MobileCancelToken::new())
            .await
            .expect("screenshot output");
        assert_eq!(output, MobileOutput::Screenshot(b"png-bytes".to_vec()));
        responder.await.expect("responder");
    }

    #[tokio::test]
    async fn backend_preserves_control_effect_on_swipe_request() {
        let (backend, mut commands, _close) = connected_backend().await;
        let responder = tokio::spawn(async move {
            let ActorCommand::Request { body, reply, .. } =
                commands.recv().await.expect("capability request");
            assert_eq!(body["type"], "a11y.swipe");
            assert_eq!(body["x1"], 1);
            assert_eq!(body["y2"], 4);
            assert_eq!(body["ms"], DEFAULT_SWIPE_MS);
            assert_eq!(body["control"], true);
            reply
                .send(Ok(json!({"type": "ack", "ok": true})))
                .expect("reply receiver");
        });
        let output = backend
            .execute(
                &MobileAction::Swipe {
                    from: Point { x: 1, y: 2 },
                    to: Point { x: 3, y: 4 },
                },
                &MobileCancelToken::new(),
            )
            .await
            .expect("swipe output");
        assert_eq!(output, MobileOutput::Ack);
        responder.await.expect("responder");
    }

    #[tokio::test]
    async fn backend_emits_remaining_apk_request_shapes_exactly() {
        let (backend, mut commands, _close) = connected_backend().await;
        let responder = tokio::spawn(async move {
            let cases = [
                (
                    json!({"type": "a11y.snapshot"}),
                    json!({"type": "a11yTree", "nodes": []}),
                ),
                (
                    json!({"type": "a11y.tap", "x": 10, "y": 20, "control": true}),
                    json!({"type": "ack", "ok": true}),
                ),
                (
                    json!({"type": "a11y.text", "text": "hello", "control": true}),
                    json!({"type": "ack", "ok": true}),
                ),
                (
                    json!({"type": "app.open", "pkg": "com.example", "control": true}),
                    json!({"type": "ack", "ok": true}),
                ),
                (
                    json!({"type": "sms.list", "sinceMs": 1000, "limit": 5}),
                    json!({"type": "smsList", "messages": []}),
                ),
            ];
            for (expected, response) in cases {
                let ActorCommand::Request { body, reply, .. } =
                    commands.recv().await.expect("capability request");
                assert_eq!(body, expected);
                reply.send(Ok(response)).expect("reply receiver");
            }
        });
        let cancel = MobileCancelToken::new();
        assert_eq!(
            backend
                .execute(&MobileAction::A11yTree {}, &cancel)
                .await
                .expect("snapshot"),
            MobileOutput::A11yTree(Vec::new())
        );
        for action in [
            MobileAction::Tap {
                element_id: None,
                x: Some(10),
                y: Some(20),
            },
            MobileAction::Type {
                text: "hello".into(),
            },
            MobileAction::OpenApp {
                package: Some("com.example".into()),
                name: None,
            },
        ] {
            assert_eq!(
                backend
                    .execute(&action, &cancel)
                    .await
                    .expect("control ack"),
                MobileOutput::Ack
            );
        }
        backend.state.record_incoming_sms(IncomingSmsPush {
            _push_type: "sms.incoming".into(),
            address: "+1555".into(),
            body: "just arrived".into(),
            ts: 1500,
        });
        let sms = backend
            .execute(
                &MobileAction::SmsRead {
                    folder: Some("inbox".into()),
                    since: Some("1000".into()),
                    limit: Some(5),
                },
                &cancel,
            )
            .await
            .expect("SMS list");
        assert!(
            matches!(sms, MobileOutput::SmsList(messages) if messages.len() == 1 && messages[0].body == "just arrived")
        );
        responder.await.expect("responder");
    }

    #[tokio::test]
    async fn known_missing_apk_grant_is_a_typed_prepare_failure() {
        let (backend, _commands, _close) = connected_backend().await;
        backend
            .state
            .capabilities_seen
            .store(false, Ordering::Release);
        assert!(matches!(
            backend
                .prepare(&MobileAction::A11yTree {}, &MobileCancelToken::new())
                .await,
            Err(MobileError::Unavailable { .. })
        ));
        backend.state.set_capabilities(vec!["accessibility".into()]);
        assert!(
            backend
                .prepare(&MobileAction::A11yTree {}, &MobileCancelToken::new())
                .await
                .is_ok()
        );
        let error = backend
            .prepare(&MobileAction::Screenshot {}, &MobileCancelToken::new())
            .await
            .expect_err("missing screen grant");
        assert!(matches!(error, MobileError::Unavailable { .. }));
    }

    #[tokio::test]
    async fn false_ack_is_a_typed_backend_failure() {
        let (backend, mut commands, _close) = connected_backend().await;
        let responder = tokio::spawn(async move {
            let ActorCommand::Request { reply, .. } =
                commands.recv().await.expect("capability request");
            reply
                .send(Ok(json!({"type": "ack", "ok": false})))
                .expect("reply receiver");
        });
        let error = backend
            .execute(
                &MobileAction::Type {
                    text: "hello".into(),
                },
                &MobileCancelToken::new(),
            )
            .await
            .expect_err("false ack");
        assert!(matches!(error, MobileError::Backend { .. }));
        responder.await.expect("responder");
    }

    #[tokio::test]
    async fn disconnected_backend_is_a_typed_unavailable_failure() {
        let backend = ApkMobileBackend {
            state: Arc::new(TransportState::new()),
        };
        let error = backend
            .execute(&MobileAction::Screenshot {}, &MobileCancelToken::new())
            .await
            .expect_err("disconnected backend");
        assert!(matches!(error, MobileError::Unavailable { .. }));
    }

    #[tokio::test(start_paused = true)]
    async fn backend_timeout_is_a_typed_failure() {
        let (backend, _commands, _close) = connected_backend().await;
        let error = backend
            .execute(&MobileAction::Screenshot {}, &MobileCancelToken::new())
            .await
            .expect_err("request timeout");
        assert!(matches!(error, MobileError::Backend { message } if message.contains("timed out")));
    }

    struct StreamingTestBridge;

    #[async_trait]
    impl MobileChatBridge for StreamingTestBridge {
        async fn handle(
            &self,
            command: ChatCommand,
            responder: ChatResponder,
        ) -> Result<(), MobileChatError> {
            assert!(matches!(command, ChatCommand::Send { text } if text == "hello"));
            responder
                .send(ChatEvent::Delta {
                    text: "checking".into(),
                    segment: "thinking",
                })
                .await?;
            responder
                .send(ChatEvent::Delta {
                    text: "world".into(),
                    segment: "answer",
                })
                .await?;
            responder.send(ChatEvent::Done).await
        }
    }

    #[tokio::test]
    async fn chat_send_streams_same_id_delta_and_done_shapes() {
        let state = Arc::new(TransportState::new());
        state.install_chat_bridge(Arc::new(StreamingTestBridge));
        let (output, mut frames) = mpsc::channel(CHAT_OUTPUT_CAPACITY);
        let (commands, command_receiver) = mpsc::channel(CHAT_COMMAND_CAPACITY);
        let (stop, stop_receiver) = watch::channel(false);
        let worker = tokio::spawn(run_bridge_commands(command_receiver, stop_receiver));
        dispatch_bridge_frame(
            Envelope {
                id: 77,
                body: json!({"type": "chat.send", "text": "hello"}),
            },
            &state,
            &output,
            &commands,
        )
        .await
        .expect("dispatch chat request");
        assert_eq!(
            frames.recv().await.expect("thinking delta"),
            Envelope {
                id: 77,
                body: json!({
                    "type": "chat.delta",
                    "text": "checking",
                    "segment": "thinking",
                }),
            }
        );
        assert_eq!(
            frames.recv().await.expect("answer delta"),
            Envelope {
                id: 77,
                body: json!({
                    "type": "chat.delta",
                    "text": "world",
                    "segment": "answer",
                }),
            }
        );
        assert_eq!(
            frames.recv().await.expect("done"),
            Envelope {
                id: 77,
                body: json!({"type": "chat.done"}),
            }
        );
        stop.send_replace(true);
        worker.await.expect("stop chat worker");
    }

    #[tokio::test]
    async fn monitor_chat_stream_uses_negative_id_delta_then_done() {
        let (mut daemon, mut apk) = tokio::io::duplex(8 * 1024);
        write_monitor_chat_stream(
            &mut daemon,
            MonitorChatStream {
                id: -9,
                text: "SMS from +1555:\nship it".into(),
            },
        )
        .await
        .expect("write monitor chat stream");
        assert_eq!(
            read_frame(&mut apk).await.expect("monitor delta"),
            Envelope {
                id: -9,
                body: json!({
                    "type": "chat.delta",
                    "text": "SMS from +1555:\nship it",
                    "segment": "answer",
                }),
            }
        );
        assert_eq!(
            read_frame(&mut apk).await.expect("monitor done"),
            Envelope {
                id: -9,
                body: json!({"type": "chat.done"}),
            }
        );
    }

    struct OrderedTestBridge;

    #[async_trait]
    impl MobileChatBridge for OrderedTestBridge {
        async fn handle(
            &self,
            command: ChatCommand,
            responder: ChatResponder,
        ) -> Result<(), MobileChatError> {
            let ChatCommand::Send { text } = command else {
                return Err(MobileChatError::invalid("expected chat.send"));
            };
            if text == "first" {
                tokio::task::yield_now().await;
            }
            responder.send(ChatEvent::Done).await
        }
    }

    #[tokio::test]
    async fn bridge_worker_preserves_socket_request_order() {
        let state = Arc::new(TransportState::new());
        state.install_chat_bridge(Arc::new(OrderedTestBridge));
        let (output, mut frames) = mpsc::channel(CHAT_OUTPUT_CAPACITY);
        let (commands, command_receiver) = mpsc::channel(CHAT_COMMAND_CAPACITY);
        let (stop, stop_receiver) = watch::channel(false);
        let worker = tokio::spawn(run_bridge_commands(command_receiver, stop_receiver));
        for (id, text) in [(41, "first"), (42, "second")] {
            dispatch_bridge_frame(
                Envelope {
                    id,
                    body: json!({"type": "chat.send", "text": text}),
                },
                &state,
                &output,
                &commands,
            )
            .await
            .expect("enqueue ordered chat request");
        }
        assert_eq!(frames.recv().await.expect("first terminal").id, 41);
        assert_eq!(frames.recv().await.expect("second terminal").id, 42);
        stop.send_replace(true);
        worker.await.expect("stop ordered chat worker");
    }

    #[tokio::test]
    #[ignore = "requires a host that permits numeric loopback socket binds"]
    async fn host_loopback_gate_authenticates_and_advertises_capabilities() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut server = MobileTransportServer::start(directory.path(), None)
            .await
            .expect("start loopback server");
        let token = std::fs::read_to_string(directory.path().join(TOKEN_FILE_NAME))
            .expect("read bootstrap token");
        let mut rejected = TcpStream::connect(server.address())
            .await
            .expect("connect rejected client");
        write_frame(
            &mut rejected,
            &Envelope {
                id: 1,
                body: json!({"type": "hello", "token": "wrong", "apkVersion": "test"}),
            },
        )
        .await
        .expect("write rejected hello");
        let rejection = read_frame(&mut rejected)
            .await
            .expect("read auth rejection");
        assert_eq!(rejection.id, 1);
        assert_eq!(rejection.body["type"], "authReject");
        let mut closed = [0_u8; 1];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), rejected.read(&mut closed))
                .await
                .expect("auth rejection close timeout")
                .expect("read auth rejection close"),
            0
        );

        let mut stream = TcpStream::connect(server.address())
            .await
            .expect("connect loopback client");
        write_frame(
            &mut stream,
            &Envelope {
                id: 1,
                body: json!({"type": "hello", "token": token, "apkVersion": "test"}),
            },
        )
        .await
        .expect("write hello");
        let response = read_frame(&mut stream).await.expect("read auth response");
        assert_eq!(response.id, 1);
        assert_eq!(response.body["type"], "authOk");
        assert!(response.body["capabilities"].is_array());
        server.shutdown().await;
    }
}
