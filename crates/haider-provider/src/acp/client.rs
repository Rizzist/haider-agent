//! The supervised ACP connection: framing, JSON-RPC correlation, inbound
//! request handling, bounded stderr drain, and child lifecycle.
//!
//! The connection is generic over an `AsyncRead`/`AsyncWrite` pair so a test
//! can drive a complete protocol exchange over `tokio::io::duplex` with no
//! subprocess at all; [`AcpConnection::spawn`] is the production constructor
//! that supervises a real child.

use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::acp::codec::{ACP_MAX_FRAME_BYTES, FrameError, LineFramer, encode_frame};
use crate::acp::wire::{
    ACP_ERROR_AUTH_REQUIRED, ACP_ERROR_METHOD_NOT_FOUND, ACP_ERROR_REQUEST_CANCELLED,
    ACP_ERROR_RESOURCE_NOT_FOUND, ACP_PROTOCOL_VERSION, AuthenticateRequest, CancelNotification,
    ClientCapabilities, ClientInfo, ContentBlock, FsCapabilities, FsReadTextFileRequest,
    FsReadTextFileResponse, FsWriteTextFileRequest, InboundFrame, InitializeRequest,
    InitializeResponse, JsonRpcError, JsonRpcErrorReply, JsonRpcId, JsonRpcNotification,
    JsonRpcRequest, JsonRpcResultReply, METHOD_AUTHENTICATE, METHOD_FS_READ_TEXT_FILE,
    METHOD_FS_WRITE_TEXT_FILE, METHOD_INITIALIZE, METHOD_SESSION_CANCEL, METHOD_SESSION_NEW,
    METHOD_SESSION_PROMPT, METHOD_SESSION_REQUEST_PERMISSION, METHOD_SESSION_SET_CONFIG_OPTION,
    METHOD_SESSION_UPDATE, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse,
    RequestPermissionRequest, RequestPermissionResponse, SessionNotification, SessionUpdate,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, StopReason,
};
use crate::{ProviderError, ProviderErrorKind};

// ---------------------------------------------------------------------------
// Derived bounds
// ---------------------------------------------------------------------------

/// Maximum concurrently outstanding agent-side requests on one connection.
///
/// Derivation. One connection supervises one Antigravity session, and Haider
/// keeps at most ONE agent-side request in flight on it: `initialize`,
/// `authenticate` and `session/new` are strictly sequential during setup, and
/// `session/prompt` is the only in-turn call. The bound is set two powers of
/// two above that single call — 1 * 64 = 64 — so a correct client can never
/// reach it while a leaked correlator or a runaway caller is refused with a
/// typed error long before the map costs real memory:
/// 64 entries * (8-byte key + one oneshot sender, ~64 bytes) = ~4.6 KiB.
pub const ACP_MAX_PENDING_REQUESTS: usize = 64;

/// Maximum stderr bytes retained for diagnostics.
///
/// Derivation. The agent's stderr is glog-formatted noise
/// (`I0904 12:03:12.535072 ... main.py:80] Starting AGY ACP Server...`) whose
/// lines measure ~80-120 bytes; 128 bytes/line is the rounded-up worst case.
/// A startup failure is explained by its last few dozen lines, so the ring
/// keeps 64 of them: 128 bytes * 64 lines = 8192 bytes = 8 KiB. The drain task
/// reads continuously, so a noisy child can neither block the stdout reader
/// nor grow this ring past the bound.
pub const ACP_STDERR_TAIL_BYTES: usize = 8 * 1024;

/// Read-chunk size for both child streams.
///
/// Derivation. One `agent_message_chunk` frame is a JSON envelope (~120 bytes)
/// around a model text delta; Haider's own reply arena segments are sized in
/// single-KiB units, so a 16 KiB chunk absorbs roughly a hundred consecutive
/// deltas per syscall: 16384 / 160 = 102. Larger chunks only add idle
/// residency per connection.
const ACP_READ_CHUNK_BYTES: usize = 16 * 1024;

/// Budget for the `initialize` handshake with a freshly spawned child.
///
/// Derivation. Cold start to first response was MEASURED at 14.75 s against
/// the real darwin-arm64 1.1.1 binary (885 MiB extracted, two Mach-O images).
/// A cold page cache on a slower or network-backed home directory is the
/// realistic worst case, so the measurement is tripled: 14.75 * 3 = 44.25 s,
/// rounded to 45 s.
pub const ACP_INITIALIZE_TIMEOUT: Duration = Duration::from_secs(45);

/// Budget for `authenticate`.
///
/// Derivation. `oauth-personal` opens a loopback OAuth flow in which a HUMAN
/// must open a browser, choose a Google account and consent. Haider's longest
/// existing no-content budget is the provider semantic-progress timeout of
/// 5 minutes (`GeminiTransportConfig::semantic_progress_timeout`, 5 * 60 s),
/// which is also inside Google's own authorization-code lifetime; the same
/// 300 s is used here rather than inventing a second human-scale constant.
pub const ACP_AUTHENTICATE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Budget for `session/new`.
///
/// Derivation. The agent answers this from local state plus at most one Google
/// catalog round trip. Haider's Gemini adapter allows 30 s to open a response
/// from Google over HTTP (`GeminiTransportConfig::response_open_timeout`);
/// the same 30 s covers the identical round trip made one process hop away.
pub const ACP_NEW_SESSION_TIMEOUT: Duration = Duration::from_secs(30);

/// Budget for `session/set_config_option`.
///
/// Derivation. Selecting a model is the same class of work as opening the
/// session that published the selector: agent-local state plus at most one
/// Google round trip to validate the value. It therefore takes the identical
/// budget as [`ACP_NEW_SESSION_TIMEOUT`] — Haider's 30 s Gemini
/// response-open bound (`GeminiTransportConfig::response_open_timeout`) —
/// rather than a second number for the same round trip.
pub const ACP_SET_CONFIG_OPTION_TIMEOUT: Duration = ACP_NEW_SESSION_TIMEOUT;

/// Budget from `session/prompt` to the FIRST inbound frame for that session.
///
/// Derivation. Two Google round trips at the Gemini response-open budget:
/// the agent may make one planning request before it emits any chunk, plus the
/// request that actually streams. 30 s * 2 = 60 s.
pub const ACP_PROMPT_OPEN_TIMEOUT: Duration = Duration::from_secs(60);

/// Budget BETWEEN inbound frames once a turn has started.
///
/// Derivation. Gemini's `chunk_idle_timeout` is 90 s for a pure token stream,
/// but an ACP agent runs its OWN tools between chunks (a build, a test run),
/// so the gap is legitimately longer. The bound is raised to Haider's
/// semantic-progress budget of 5 minutes
/// (`GeminiTransportConfig::semantic_progress_timeout`, 5 * 60 s), which is
/// already the longest content-free gap the turn engine tolerates.
pub const ACP_PROMPT_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Grace between `session/cancel` and terminating the child.
///
/// Derivation. Cancellation only requires the agent to abandon in-flight work
/// and answer the outstanding `session/prompt`; it is not a network round
/// trip. The shortest transport bound in the repo is Gemini's 10 s
/// `connect_timeout`, which is the right order for one local abort
/// acknowledgement.
pub const ACP_CANCEL_GRACE: Duration = Duration::from_secs(10);

/// Grace between polite termination and the forced kill.
///
/// Derivation. On shutdown the agent flushes `$GEMINI_HOME/antigravity-acp/`
/// (`settings.json`, `acp_token.json`). One durable local file write is
/// single-digit milliseconds — this repo measures `F_FULLFSYNC` at ~4 ms — so
/// 2000 ms / 4 ms = 500 times the write budget, which absorbs a loaded disk
/// without letting a wedged child hold the slot.
pub const ACP_TERMINATE_GRACE: Duration = Duration::from_secs(2);

/// Budget for reaping the child after the forced kill.
///
/// Derivation. Post-kill reaping is kernel bookkeeping, not work. The daemon's
/// hook supervisor already uses 1 s for exactly this operation
/// (`HOOK_CHILD_REAP_TIMEOUT`), and this path matches it.
pub const ACP_CHILD_REAP_TIMEOUT: Duration = Duration::from_secs(1);

/// Ambient environment variables whose operator-scoped value must never be
/// INHERITED by the child.
///
/// The child environment is BUILT from an allowlist rather than inherited, so
/// this list is documentation and a test assertion rather than the enforcement
/// mechanism: an allowlist cannot leak a name that is not on it. `GEMINI_HOME`
/// appears here because the ambient value must not survive — the launcher
/// substitutes the per-account profile directory. `CLOUDSDK_*` is matched by
/// prefix.
pub const ACP_STRIPPED_ENVIRONMENT_NAMES: [&str; 9] = [
    "GEMINI_HOME",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "GOOGLE_CLOUD_PROJECT",
    "AGY_ACP_CCPA_PROJECT",
    "AGY_ACP_ENABLE_OAUTH",
    "ANTIGRAVITY_HARNESS_PATH",
    "BROWSER",
];

/// Prefix families stripped wholesale, e.g. every `gcloud` configuration
/// variable.
pub const ACP_STRIPPED_ENVIRONMENT_PREFIXES: [&str; 1] = ["CLOUDSDK_"];

/// Ambient variables the child is allowed to keep.
const ACP_INHERITED_ENVIRONMENT_NAMES: [&str; 5] = ["PATH", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE"];

/// Exact prefix the agent prints before its OAuth URL. In 1.1.1 it appears on
/// stderr; earlier builds printed it on stdout. Any line carrying it is
/// replaced in the diagnostics ring: the URL, its query, the code and any
/// token material are never logged, journalled, or put in an error message.
pub const ACP_OAUTH_URL_LINE_PREFIX: &str =
    "Open the following link to authenticate the ACP server: ";

/// What replaces a redacted OAuth line in the stderr tail.
pub const ACP_OAUTH_URL_REDACTION: &str = "<redacted acp oauth url>";

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Typed ACP transport/protocol failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcpError {
    /// A recoverable framing failure that was escalated because it aborted a
    /// turn (the reader itself skips these and keeps going).
    Frame(FrameError),
    /// An operating-system read/write failure on the child pipes.
    Transport(String),
    /// The child's stdout reached EOF, or the connection was shut down, while
    /// a request or turn was outstanding.
    Closed,
    Timeout {
        operation: &'static str,
        limit: Duration,
    },
    PendingLimit {
        limit: usize,
    },
    /// The agent answered a request with a JSON-RPC error object.
    Rpc(JsonRpcError),
    /// A well-formed frame whose payload did not match the negotiated schema.
    Decode(&'static str),
    ProtocolVersion {
        expected: u16,
        actual: u16,
    },
    /// `oauth-personal` was not advertised. Carries the ids that WERE, so the
    /// operator can see what the agent offered.
    AuthMethodUnavailable {
        advertised: Vec<String>,
    },
}

impl std::fmt::Display for AcpError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Frame(error) => write!(formatter, "{error}"),
            Self::Transport(detail) => {
                write!(formatter, "the ACP agent transport failed: {detail}")
            }
            Self::Closed => formatter.write_str("the ACP agent closed its stdout"),
            Self::Timeout { operation, limit } => write!(
                formatter,
                "the ACP agent did not answer {operation} within {} ms",
                limit.as_millis()
            ),
            Self::PendingLimit { limit } => write!(
                formatter,
                "more than {limit} ACP requests were outstanding on one connection"
            ),
            Self::Rpc(error) => write!(formatter, "the ACP agent returned an error: {error}"),
            Self::Decode(detail) => write!(formatter, "the ACP agent sent {detail}"),
            Self::ProtocolVersion { expected, actual } => write!(
                formatter,
                "the ACP agent negotiated protocol version {actual}, but Haider speaks {expected}"
            ),
            Self::AuthMethodUnavailable { advertised } => write!(
                formatter,
                "the ACP agent does not advertise the oauth-personal auth method; it advertised: {}",
                if advertised.is_empty() {
                    "none".to_owned()
                } else {
                    advertised.join(", ")
                }
            ),
        }
    }
}

impl AcpError {
    fn kind(&self) -> ProviderErrorKind {
        match self {
            Self::Frame(_) | Self::Decode(_) => ProviderErrorKind::MalformedFrame,
            Self::Transport(_) => ProviderErrorKind::Transport,
            Self::Closed => ProviderErrorKind::StreamInterrupted,
            Self::Timeout { .. } => ProviderErrorKind::Transport,
            Self::PendingLimit { .. } => ProviderErrorKind::Internal,
            Self::ProtocolVersion { .. } => ProviderErrorKind::ConnectionConfiguration,
            Self::AuthMethodUnavailable { .. } => ProviderErrorKind::Authentication,
            Self::Rpc(error) => match error.code {
                ACP_ERROR_AUTH_REQUIRED => ProviderErrorKind::Authentication,
                ACP_ERROR_RESOURCE_NOT_FOUND | ACP_ERROR_METHOD_NOT_FOUND => {
                    ProviderErrorKind::InvalidRequest
                }
                _ => ProviderErrorKind::Transport,
            },
        }
    }

    /// Converts to the crate's terminal error type, attaching the bounded
    /// stderr tail as operator detail. The tail is already OAuth-redacted by
    /// [`StderrRing`].
    pub fn into_provider_error(self, stderr_tail: &str) -> ProviderError {
        let kind = self.kind();
        let message = self.to_string();
        let error = ProviderError::new(kind, message.clone());
        if stderr_tail.is_empty() {
            error
        } else {
            error.with_provider_detail(&format!("{message} Agent stderr tail: {stderr_tail}"))
        }
    }
}

// ---------------------------------------------------------------------------
// Child environment
// ---------------------------------------------------------------------------

/// Builds the child's COMPLETE environment from an allowlist.
///
/// Nothing is inherited implicitly: the caller passes the ambient environment
/// and only [`ACP_INHERITED_ENVIRONMENT_NAMES`] survive, which is what makes
/// every name in [`ACP_STRIPPED_ENVIRONMENT_NAMES`] and every
/// `CLOUDSDK_*` variable structurally unreachable. Pure, so a test can assert
/// the resulting map exactly.
pub fn acp_child_environment<I>(
    profile_dir: &Path,
    home_dir: &Path,
    ambient: I,
) -> BTreeMap<OsString, OsString>
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    let mut environment = BTreeMap::new();
    for (name, value) in ambient {
        if ACP_INHERITED_ENVIRONMENT_NAMES
            .iter()
            .any(|allowed| name == OsStr::new(allowed))
        {
            environment.insert(name, value);
        }
    }
    // The agent resolves its profile from `$GEMINI_HOME` and writes
    // `$GEMINI_HOME/antigravity-acp/{settings,acp_token}.json` there, so this
    // is the per-account isolation boundary.
    environment.insert(OsString::from("GEMINI_HOME"), profile_dir.into());
    environment.insert(OsString::from("HOME"), home_dir.into());
    // Forces file token storage so two Haider accounts cannot collide on one
    // OS-keychain entry.
    environment.insert(
        OsString::from("AGY_ACP_FORCE_FILE_STORAGE"),
        OsString::from("1"),
    );
    // The agent is a Python program; unbuffered stdio keeps its stdout frames
    // and stderr diagnostics from being withheld behind a block buffer.
    environment.insert(OsString::from("PYTHONUNBUFFERED"), OsString::from("1"));
    environment
}

// ---------------------------------------------------------------------------
// Bounded stderr ring
// ---------------------------------------------------------------------------

/// Bounded tail of the child's stderr, with OAuth URL lines redacted on the
/// way in.
#[derive(Debug)]
pub struct StderrRing {
    capacity: usize,
    state: Mutex<StderrRingState>,
}

#[derive(Debug, Default)]
struct StderrRingState {
    retained: Vec<u8>,
    partial: Vec<u8>,
}

impl StderrRing {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            state: Mutex::new(StderrRingState::default()),
        }
    }

    /// Appends one read, retaining at most `capacity` bytes.
    ///
    /// Lines are reassembled before retention so the OAuth prefix can be
    /// matched even when it straddles two reads. The line accumulator is
    /// itself bounded by `capacity`: a child that never emits a newline is
    /// truncated instead of growing the ring.
    pub fn push(&self, chunk: &[u8]) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        for byte in chunk {
            if *byte == b'\n' {
                let line = std::mem::take(&mut state.partial);
                self.retain_line(&mut state, &line);
            } else if state.partial.len() < self.capacity {
                state.partial.push(*byte);
            }
        }
    }

    fn retain_line(&self, state: &mut StderrRingState, line: &[u8]) {
        let text = String::from_utf8_lossy(line);
        let rendered = if text.trim_start().starts_with(ACP_OAUTH_URL_LINE_PREFIX) {
            ACP_OAUTH_URL_REDACTION
        } else {
            text.as_ref()
        };
        state.retained.extend_from_slice(rendered.as_bytes());
        state.retained.push(b'\n');
        if state.retained.len() > self.capacity {
            let excess = state.retained.len() - self.capacity;
            state.retained.drain(..excess);
        }
    }

    /// The retained tail, lossily decoded. Never contains an OAuth URL.
    pub fn tail(&self) -> String {
        let Ok(state) = self.state.lock() else {
            return String::new();
        };
        String::from_utf8_lossy(&state.retained).trim().to_owned()
    }

    #[cfg(test)]
    pub fn retained_len(&self) -> usize {
        self.state.lock().map_or(0, |state| state.retained.len())
    }

    #[cfg(test)]
    pub fn partial_len(&self) -> usize {
        self.state.lock().map_or(0, |state| state.partial.len())
    }
}

// ---------------------------------------------------------------------------
// Inbound request handler
// ---------------------------------------------------------------------------

/// How Haider answers the requests the AGENT makes of its client.
///
/// The daemon will later map these onto Haider's own permission engine and
/// workspace containment. Until then the default implementation refuses every
/// one of them, because [`ClientCapabilities`] declares `fs` and `terminal`
/// false and a client must never advertise a capability it cannot enforce.
#[async_trait]
pub trait AcpClientHandler: Send + Sync {
    async fn request_permission(
        &self,
        request: RequestPermissionRequest,
    ) -> Result<RequestPermissionResponse, JsonRpcError>;

    async fn read_text_file(
        &self,
        request: FsReadTextFileRequest,
    ) -> Result<FsReadTextFileResponse, JsonRpcError>;

    async fn write_text_file(&self, request: FsWriteTextFileRequest) -> Result<(), JsonRpcError>;
}

/// The default handler: refuses everything with a JSON-RPC error.
#[derive(Debug, Default)]
pub struct RefusingAcpClientHandler;

#[async_trait]
impl AcpClientHandler for RefusingAcpClientHandler {
    async fn request_permission(
        &self,
        _request: RequestPermissionRequest,
    ) -> Result<RequestPermissionResponse, JsonRpcError> {
        // Auto-allowing would be a silent grant and auto-rejecting would
        // invent a policy Haider has not been given, so the honest answer is
        // that no permission authority is bound to this connection yet.
        Err(JsonRpcError::method_not_found(
            METHOD_SESSION_REQUEST_PERMISSION,
        ))
    }

    async fn read_text_file(
        &self,
        _request: FsReadTextFileRequest,
    ) -> Result<FsReadTextFileResponse, JsonRpcError> {
        Err(JsonRpcError::method_not_found(METHOD_FS_READ_TEXT_FILE))
    }

    async fn write_text_file(&self, _request: FsWriteTextFileRequest) -> Result<(), JsonRpcError> {
        Err(JsonRpcError::method_not_found(METHOD_FS_WRITE_TEXT_FILE))
    }
}

// ---------------------------------------------------------------------------
// Prompt stream
// ---------------------------------------------------------------------------

/// One event routed to an open turn. The reader task is a single sequential
/// consumer, so an update queued before a terminal is always observed first.
#[derive(Debug)]
pub enum PromptEvent {
    Update(SessionUpdate),
    Finished(StopReason),
    Failed(AcpError),
}

/// The ordered event stream of one open `session/prompt`.
pub struct PromptStream {
    connection: Arc<AcpConnection>,
    session_id: String,
    events: mpsc::Receiver<PromptEvent>,
}

impl PromptStream {
    pub async fn recv(&mut self) -> Option<PromptEvent> {
        self.events.recv().await
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn connection(&self) -> &Arc<AcpConnection> {
        &self.connection
    }
}

impl Drop for PromptStream {
    fn drop(&mut self) {
        self.connection.unsubscribe_session(&self.session_id);
    }
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

enum Pending {
    /// An ordinary request/response correlation.
    Call(oneshot::Sender<Result<serde_json::Value, AcpError>>),
    /// An open `session/prompt`: its response is the turn's terminal and is
    /// routed into the session channel so it can never overtake a queued
    /// update.
    Prompt(String),
}

#[derive(Default)]
struct ConnectionState {
    pending: HashMap<u64, Pending>,
    sessions: HashMap<String, mpsc::Sender<PromptEvent>>,
    /// Inbound `session/request_permission` ids Haider has not answered yet,
    /// keyed so a `session/cancel` can settle them all.
    open_permissions: HashMap<JsonRpcId, String>,
    /// Sessions for which `session/cancel` has been sent. A permission request
    /// that arrives after the cancel is answered `cancelled` immediately.
    cancelled_sessions: Vec<String>,
    closed: bool,
}

/// A supervised ACP peer.
pub struct AcpConnection {
    writer: AsyncMutex<Box<dyn AsyncWrite + Send + Unpin>>,
    state: Mutex<ConnectionState>,
    next_id: AtomicU64,
    stderr: Arc<StderrRing>,
    child: AsyncMutex<Option<SupervisedChild>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl std::fmt::Debug for AcpConnection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AcpConnection")
            .finish_non_exhaustive()
    }
}

impl Drop for AcpConnection {
    fn drop(&mut self) {
        if let Ok(tasks) = self.tasks.lock() {
            for task in tasks.iter() {
                task.abort();
            }
        }
    }
}

/// How a supervised child ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcpChildReap {
    /// The child exited and was reaped.
    Exited,
    /// No child was supervised (a duplex-driven connection).
    NoChild,
    /// The child did not exit within [`ACP_CHILD_REAP_TIMEOUT`] after the
    /// forced kill. The `kill_on_drop` guard remains the last resort.
    Unreaped,
}

struct SupervisedChild {
    child: tokio::process::Child,
    pid: Option<u32>,
}

/// Everything needed to launch the real agent.
#[derive(Debug, Clone)]
pub struct AcpLaunchSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// The per-account profile directory exported as `GEMINI_HOME`.
    pub profile_dir: PathBuf,
    /// The `HOME` the child sees. Callers point this at the per-account
    /// profile root so the child cannot read the operator's real home.
    pub home_dir: PathBuf,
    pub working_dir: PathBuf,
}

impl AcpConnection {
    /// Connects over a caller-supplied transport pair. No subprocess is
    /// involved, so tests drive a complete exchange over `tokio::io::duplex`.
    pub fn connect<R, W>(reader: R, writer: W, handler: Arc<dyn AcpClientHandler>) -> Arc<Self>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        Self::assemble(reader, writer, handler, None, None)
    }

    /// Connects over a caller-supplied transport pair AND a separate
    /// diagnostics stream drained into the bounded stderr ring.
    pub fn connect_with_stderr<R, W, S>(
        reader: R,
        writer: W,
        stderr: S,
        handler: Arc<dyn AcpClientHandler>,
    ) -> Arc<Self>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
        S: AsyncRead + Send + Unpin + 'static,
    {
        Self::assemble(reader, writer, handler, Some(Box::new(stderr)), None)
    }

    /// Spawns the real agent and connects to its stdio.
    pub fn spawn(
        spec: &AcpLaunchSpec,
        handler: Arc<dyn AcpClientHandler>,
    ) -> Result<Arc<Self>, AcpError> {
        let mut command = tokio::process::Command::new(&spec.program);
        command
            .args(&spec.args)
            .current_dir(&spec.working_dir)
            .env_clear()
            .envs(acp_child_environment(
                &spec.profile_dir,
                &spec.home_dir,
                std::env::vars_os(),
            ))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|error| AcpError::Transport(format!("{}", error.kind())))?;
        let pid = child.id();
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err(AcpError::Transport(
                "the ACP child did not expose piped stdio".to_owned(),
            ));
        };
        Ok(Self::assemble(
            stdout,
            stdin,
            handler,
            Some(Box::new(stderr)),
            Some(SupervisedChild { child, pid }),
        ))
    }

    fn assemble<R, W>(
        reader: R,
        writer: W,
        handler: Arc<dyn AcpClientHandler>,
        stderr: Option<Box<dyn AsyncRead + Send + Unpin>>,
        child: Option<SupervisedChild>,
    ) -> Arc<Self>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let connection = Arc::new(Self {
            writer: AsyncMutex::new(Box::new(writer)),
            state: Mutex::new(ConnectionState::default()),
            next_id: AtomicU64::new(1),
            stderr: Arc::new(StderrRing::new(ACP_STDERR_TAIL_BYTES)),
            child: AsyncMutex::new(child),
            tasks: Mutex::new(Vec::new()),
        });
        let mut tasks = Vec::with_capacity(2);
        let reader_connection = Arc::clone(&connection);
        tasks.push(tokio::spawn(async move {
            read_loop(reader_connection, reader, handler).await;
        }));
        if let Some(stderr) = stderr {
            let ring = Arc::clone(&connection.stderr);
            tasks.push(tokio::spawn(async move {
                drain_stderr(ring, stderr).await;
            }));
        }
        if let Ok(mut slot) = connection.tasks.lock() {
            *slot = tasks;
        }
        connection
    }

    pub fn stderr_tail(&self) -> String {
        self.stderr.tail()
    }

    #[cfg(test)]
    pub fn stderr_ring(&self) -> &Arc<StderrRing> {
        &self.stderr
    }

    // -- typed calls --------------------------------------------------------

    pub async fn initialize(
        &self,
        client_info: ClientInfo,
    ) -> Result<InitializeResponse, AcpError> {
        let request = InitializeRequest {
            protocol_version: ACP_PROTOCOL_VERSION,
            client_capabilities: ClientCapabilities {
                fs: FsCapabilities {
                    read_text_file: false,
                    write_text_file: false,
                },
                terminal: false,
            },
            client_info: Some(client_info),
        };
        let response: InitializeResponse = self
            .call(METHOD_INITIALIZE, request, ACP_INITIALIZE_TIMEOUT)
            .await?;
        if response.protocol_version != ACP_PROTOCOL_VERSION {
            return Err(AcpError::ProtocolVersion {
                expected: ACP_PROTOCOL_VERSION,
                actual: response.protocol_version,
            });
        }
        Ok(response)
    }

    pub async fn authenticate(&self, method_id: &str) -> Result<(), AcpError> {
        let request = AuthenticateRequest {
            method_id: method_id.to_owned(),
        };
        let _ignored: serde_json::Value = self
            .call(METHOD_AUTHENTICATE, request, ACP_AUTHENTICATE_TIMEOUT)
            .await?;
        Ok(())
    }

    pub async fn new_session(
        &self,
        cwd: &str,
        additional_directories: Vec<String>,
    ) -> Result<NewSessionResponse, AcpError> {
        let request = NewSessionRequest {
            cwd: cwd.to_owned(),
            mcp_servers: Vec::new(),
            additional_directories,
        };
        self.call(METHOD_SESSION_NEW, request, ACP_NEW_SESSION_TIMEOUT)
            .await
    }

    /// Sets one session configuration option to a `SessionConfigValueId`.
    ///
    /// This is how a model is selected: ACP has no `session/set_model`. The
    /// answer is the FULL configuration-option set with current values, so the
    /// caller refreshes its cached catalog from what the agent reports rather
    /// than from what it asked for.
    pub async fn set_config_option(
        &self,
        session_id: &str,
        config_id: &str,
        value: &str,
    ) -> Result<SetSessionConfigOptionResponse, AcpError> {
        let request = SetSessionConfigOptionRequest {
            session_id: session_id.to_owned(),
            config_id: config_id.to_owned(),
            value: value.to_owned(),
        };
        self.call(
            METHOD_SESSION_SET_CONFIG_OPTION,
            request,
            ACP_SET_CONFIG_OPTION_TIMEOUT,
        )
        .await
    }

    /// Opens one turn. The session subscription is registered BEFORE the
    /// request is written, so no `session/update` can arrive unrouted.
    pub async fn open_prompt(
        self: &Arc<Self>,
        session_id: &str,
        prompt: Vec<ContentBlock>,
    ) -> Result<PromptStream, AcpError> {
        let (sender, events) = mpsc::channel(ACP_PROMPT_EVENT_CAPACITY);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        {
            let mut state = self.lock_state();
            if state.closed {
                return Err(AcpError::Closed);
            }
            if state.pending.len() >= ACP_MAX_PENDING_REQUESTS {
                return Err(AcpError::PendingLimit {
                    limit: ACP_MAX_PENDING_REQUESTS,
                });
            }
            state
                .cancelled_sessions
                .retain(|cancelled| cancelled != session_id);
            state.sessions.insert(session_id.to_owned(), sender);
            state
                .pending
                .insert(id, Pending::Prompt(session_id.to_owned()));
        }
        let request = PromptRequest {
            session_id: session_id.to_owned(),
            prompt,
        };
        if let Err(error) = self
            .write_frame(&JsonRpcRequest::new(id, METHOD_SESSION_PROMPT, request))
            .await
        {
            let mut state = self.lock_state();
            state.pending.remove(&id);
            state.sessions.remove(session_id);
            return Err(error);
        }
        Ok(PromptStream {
            connection: Arc::clone(self),
            session_id: session_id.to_owned(),
            events,
        })
    }

    /// Cancels one turn.
    ///
    /// Every still-pending `session/request_permission` is answered with the
    /// `cancelled` outcome FIRST — the schema requires it, and answering
    /// before the notification means the agent is never blocked on Haider
    /// while it tears the turn down. The session is then marked cancelled so a
    /// permission request that races the notification is settled the same way.
    pub async fn cancel(&self, session_id: &str) -> Result<(), AcpError> {
        let outstanding = {
            let mut state = self.lock_state();
            if !state
                .cancelled_sessions
                .iter()
                .any(|cancelled| cancelled == session_id)
            {
                state.cancelled_sessions.push(session_id.to_owned());
            }
            let mut outstanding = Vec::new();
            state.open_permissions.retain(|id, session| {
                if session == session_id {
                    outstanding.push(id.clone());
                    false
                } else {
                    true
                }
            });
            outstanding
        };
        for id in outstanding {
            self.write_frame(&JsonRpcResultReply::new(
                id,
                RequestPermissionResponse::cancelled(),
            ))
            .await?;
        }
        self.write_frame(&JsonRpcNotification::new(
            METHOD_SESSION_CANCEL,
            CancelNotification {
                session_id: session_id.to_owned(),
            },
        ))
        .await
    }

    /// Cancels the turn, waits a bounded grace for the agent to settle, then
    /// terminates, force-kills and ALWAYS reaps the child.
    pub async fn shutdown(&self, session_id: Option<&str>) -> AcpChildReap {
        if let Some(session_id) = session_id {
            let _ = self.cancel(session_id).await;
        }
        let mut slot = self.child.lock().await;
        let Some(supervised) = slot.as_mut() else {
            drop(slot);
            self.close(AcpError::Closed).await;
            return AcpChildReap::NoChild;
        };
        let reap = terminate_child(supervised).await;
        *slot = None;
        drop(slot);
        self.close(AcpError::Closed).await;
        reap
    }

    // -- plumbing -----------------------------------------------------------

    fn lock_state(&self) -> std::sync::MutexGuard<'_, ConnectionState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    async fn call<P, R>(&self, method: &str, params: P, budget: Duration) -> Result<R, AcpError>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        let (sender, receiver) = oneshot::channel();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        {
            let mut state = self.lock_state();
            if state.closed {
                return Err(AcpError::Closed);
            }
            if state.pending.len() >= ACP_MAX_PENDING_REQUESTS {
                return Err(AcpError::PendingLimit {
                    limit: ACP_MAX_PENDING_REQUESTS,
                });
            }
            state.pending.insert(id, Pending::Call(sender));
        }
        if let Err(error) = self
            .write_frame(&JsonRpcRequest::new(id, method, params))
            .await
        {
            self.lock_state().pending.remove(&id);
            return Err(error);
        }
        let operation = static_method_label(method);
        let value = match haider_platform::bounded_wait(operation, budget, receiver).await {
            haider_platform::BoundedWait::Completed(Ok(result)) => result?,
            haider_platform::BoundedWait::Completed(Err(_)) => {
                self.lock_state().pending.remove(&id);
                return Err(AcpError::Closed);
            }
            haider_platform::BoundedWait::TimedOut(timeout) => {
                self.lock_state().pending.remove(&id);
                return Err(AcpError::Timeout {
                    operation: timeout.operation(),
                    limit: timeout.limit(),
                });
            }
        };
        serde_json::from_value(value).map_err(|_| AcpError::Decode("an unreadable result payload"))
    }

    async fn write_frame<T: Serialize>(&self, frame: &T) -> Result<(), AcpError> {
        let bytes = encode_frame(frame).map_err(AcpError::Frame)?;
        let mut writer = self.writer.lock().await;
        writer
            .write_all(&bytes)
            .await
            .map_err(|error| AcpError::Transport(format!("{}", error.kind())))?;
        writer
            .flush()
            .await
            .map_err(|error| AcpError::Transport(format!("{}", error.kind())))
    }

    fn unsubscribe_session(&self, session_id: &str) {
        let mut state = self.lock_state();
        state.sessions.remove(session_id);
        state.pending.retain(
            |_, pending| !matches!(pending, Pending::Prompt(session) if session == session_id),
        );
    }

    /// Fails every outstanding correlator exactly once and refuses further
    /// calls. Idempotent: a second close finds the maps already drained.
    ///
    /// The terminal is delivered with backpressure rather than `try_send`: a
    /// dropped terminal would leave a turn waiting for its idle budget to
    /// expire instead of failing at the moment the child died.
    async fn close(&self, error: AcpError) {
        let (pending, sessions) = {
            let mut state = self.lock_state();
            state.closed = true;
            (
                std::mem::take(&mut state.pending),
                std::mem::take(&mut state.sessions),
            )
        };
        for entry in pending.into_values() {
            if let Pending::Call(sender) = entry {
                let _ = sender.send(Err(error.clone()));
            }
        }
        for sender in sessions.into_values() {
            let _ = sender.send(PromptEvent::Failed(error.clone())).await;
        }
    }

    #[cfg(test)]
    pub fn pending_len(&self) -> usize {
        self.lock_state().pending.len()
    }

    #[cfg(test)]
    pub async fn supervised_child_present(&self) -> bool {
        self.child.lock().await.is_some()
    }
}

/// Per-turn event queue depth.
///
/// Derivation. The reader task must never block on a slow turn consumer while
/// it still owes progress to the writer side. Every other adapter in this
/// crate sizes its provider stream at `STREAM_CAPACITY` = 32 events, and one
/// ACP update maps to at most two stream events (a tool call emits a use row
/// and a result row), so the queue is sized at the same 32: it absorbs a full
/// stream channel's worth of backlog before it applies backpressure.
const ACP_PROMPT_EVENT_CAPACITY: usize = 32;

fn static_method_label(method: &str) -> &'static str {
    match method {
        METHOD_INITIALIZE => "acp initialize",
        METHOD_AUTHENTICATE => "acp authenticate",
        METHOD_SESSION_NEW => "acp session/new",
        METHOD_SESSION_PROMPT => "acp session/prompt",
        METHOD_SESSION_SET_CONFIG_OPTION => "acp session/set_config_option",
        _ => "acp request",
    }
}

async fn terminate_child(supervised: &mut SupervisedChild) -> AcpChildReap {
    if let Some(pid) = supervised.pid {
        let _ = haider_platform::signal_process(pid, haider_platform::ProcessSignal::Terminate);
    }
    if let haider_platform::BoundedWait::Completed(Ok(_)) = haider_platform::bounded_wait(
        "acp child terminate",
        ACP_TERMINATE_GRACE,
        supervised.child.wait(),
    )
    .await
    {
        return AcpChildReap::Exited;
    }
    let _ = supervised.child.start_kill();
    match haider_platform::bounded_wait(
        "acp child reap",
        ACP_CHILD_REAP_TIMEOUT,
        supervised.child.wait(),
    )
    .await
    {
        haider_platform::BoundedWait::Completed(Ok(_)) => AcpChildReap::Exited,
        haider_platform::BoundedWait::Completed(Err(_))
        | haider_platform::BoundedWait::TimedOut(_) => AcpChildReap::Unreaped,
    }
}

async fn drain_stderr<S>(ring: Arc<StderrRing>, mut stderr: S)
where
    S: AsyncRead + Send + Unpin + 'static,
{
    let mut chunk = vec![0_u8; ACP_READ_CHUNK_BYTES];
    loop {
        match stderr.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(read) => ring.push(&chunk[..read]),
        }
    }
}

async fn read_loop<R>(
    connection: Arc<AcpConnection>,
    mut reader: R,
    handler: Arc<dyn AcpClientHandler>,
) where
    R: AsyncRead + Send + Unpin + 'static,
{
    let mut framer = LineFramer::new();
    let mut chunk = vec![0_u8; ACP_READ_CHUNK_BYTES];
    loop {
        let read = match reader.read(&mut chunk).await {
            Ok(0) => {
                connection.close(AcpError::Closed).await;
                return;
            }
            Ok(read) => read,
            Err(error) => {
                connection
                    .close(AcpError::Transport(format!("{}", error.kind())))
                    .await;
                return;
            }
        };
        framer.feed(&chunk[..read]);
        while let Some(frame) = framer.next_frame() {
            match frame {
                Ok(frame) => dispatch(&connection, &handler, frame).await,
                Err(FrameError::MalformedJson) => {
                    // Recoverable, and deliberately silent: the line may be an
                    // OAuth URL an older build printed on stdout, so neither
                    // its bytes nor a rendering of them may reach a log.
                    tracing::debug!(
                        target: "haider.provider.acp",
                        "skipped an ACP stdout line that is not a valid message"
                    );
                }
                Err(error @ FrameError::LineTooLong { .. }) => {
                    tracing::warn!(
                        target: "haider.provider.acp",
                        limit = ACP_MAX_FRAME_BYTES,
                        "{error}"
                    );
                    connection.fail_open_turns(AcpError::Frame(error)).await;
                }
            }
        }
    }
}

impl AcpConnection {
    /// Delivers one terminal failure to every open turn without closing the
    /// connection. Used for a framing fault, which is recoverable for the
    /// transport but not for a turn already mid-stream.
    async fn fail_open_turns(&self, error: AcpError) {
        let sessions = {
            let mut state = self.lock_state();
            std::mem::take(&mut state.sessions)
        };
        for sender in sessions.into_values() {
            let _ = sender.send(PromptEvent::Failed(error.clone())).await;
        }
    }

    async fn resolve_response(&self, id: u64, result: Result<serde_json::Value, AcpError>) {
        let pending = self.lock_state().pending.remove(&id);
        match pending {
            // A response to an id Haider never minted, or minted and already
            // settled, is ignored: it cannot corrupt the pending map.
            None => {}
            Some(Pending::Call(sender)) => {
                let _ = sender.send(result);
            }
            Some(Pending::Prompt(session_id)) => {
                let event = match result {
                    Ok(value) => match serde_json::from_value::<PromptResponse>(value) {
                        Ok(response) => PromptEvent::Finished(response.stop_reason),
                        Err(_) => PromptEvent::Failed(AcpError::Decode(
                            "a session/prompt response without a known stop reason",
                        )),
                    },
                    Err(error) => PromptEvent::Failed(error),
                };
                let sender = self.lock_state().sessions.remove(&session_id);
                if let Some(sender) = sender {
                    let _ = sender.send(event).await;
                }
            }
        }
    }

    /// Routes one update to its turn.
    ///
    /// The send applies BACKPRESSURE instead of dropping: an update silently
    /// discarded because the turn queue was momentarily full would be lost
    /// model output. A stalled consumer therefore stalls this reader, which is
    /// the same contract every other adapter in this crate keeps.
    async fn route_session_update(&self, notification: SessionNotification) {
        let sender = self
            .lock_state()
            .sessions
            .get(&notification.session_id)
            .cloned();
        if let Some(sender) = sender {
            let _ = sender.send(PromptEvent::Update(notification.update)).await;
        }
    }

    /// Registers an inbound permission request, or reports that its session is
    /// already cancelling and it must be answered `cancelled` at once.
    fn register_permission(&self, id: &JsonRpcId, session_id: &str) -> bool {
        let mut state = self.lock_state();
        if state
            .cancelled_sessions
            .iter()
            .any(|cancelled| cancelled == session_id)
        {
            return false;
        }
        state
            .open_permissions
            .insert(id.clone(), session_id.to_owned());
        true
    }

    /// Claims an inbound permission request so exactly one answer is written:
    /// either the handler's, or the `cancelled` outcome `cancel` already sent.
    fn claim_permission(&self, id: &JsonRpcId) -> bool {
        self.lock_state().open_permissions.remove(id).is_some()
    }
}

async fn dispatch(
    connection: &Arc<AcpConnection>,
    handler: &Arc<dyn AcpClientHandler>,
    frame: InboundFrame,
) {
    if !frame.declares_supported_version() {
        tracing::debug!(
            target: "haider.provider.acp",
            "skipped an ACP frame declaring an unsupported JSON-RPC version"
        );
        return;
    }
    match (frame.method.as_deref(), frame.id.clone()) {
        (Some(method), Some(id)) => {
            handle_inbound_request(connection, handler, method, id, frame.params).await;
        }
        (Some(METHOD_SESSION_UPDATE), None) => {
            let Some(params) = frame.params else {
                return;
            };
            match serde_json::from_value::<SessionNotification>(params) {
                Ok(notification) => connection.route_session_update(notification).await,
                Err(_) => tracing::debug!(
                    target: "haider.provider.acp",
                    "skipped an unreadable session/update notification"
                ),
            }
        }
        // Any other notification is ignored: ACP is extensible and a client
        // owes no answer to a notification it does not implement.
        (Some(_), None) => {}
        (None, Some(id)) => {
            let Some(id) = id.as_outbound() else {
                return;
            };
            let result = match (frame.result, frame.error) {
                (_, Some(error)) => Err(AcpError::Rpc(error)),
                (Some(result), None) => Ok(result),
                // A response with neither member is not a JSON-RPC response.
                (None, None) => Err(AcpError::Decode("a response with no result and no error")),
            };
            connection.resolve_response(id, result).await;
        }
        (None, None) => {}
    }
}

async fn handle_inbound_request(
    connection: &Arc<AcpConnection>,
    handler: &Arc<dyn AcpClientHandler>,
    method: &str,
    id: JsonRpcId,
    params: Option<serde_json::Value>,
) {
    let params = params.unwrap_or(serde_json::Value::Null);
    match method {
        METHOD_SESSION_REQUEST_PERMISSION => {
            let request = match serde_json::from_value::<RequestPermissionRequest>(params) {
                Ok(request) => request,
                Err(_) => {
                    reply_error(
                        connection,
                        id,
                        JsonRpcError {
                            code: ACP_ERROR_METHOD_NOT_FOUND,
                            message: "haider could not read the permission request".to_owned(),
                            data: None,
                        },
                    )
                    .await;
                    return;
                }
            };
            if !connection.register_permission(&id, &request.session_id) {
                let _ = connection
                    .write_frame(&JsonRpcResultReply::new(
                        id,
                        RequestPermissionResponse::cancelled(),
                    ))
                    .await;
                return;
            }
            // The handler runs off the reader task: a slow permission decision
            // must never stall the stdout stream it is racing.
            let connection = Arc::clone(connection);
            let handler = Arc::clone(handler);
            tokio::spawn(async move {
                let outcome = handler.request_permission(request).await;
                if !connection.claim_permission(&id) {
                    // `cancel` already answered this request with the
                    // `cancelled` outcome; a second answer is a protocol
                    // violation.
                    return;
                }
                match outcome {
                    Ok(response) => {
                        let _ = connection
                            .write_frame(&JsonRpcResultReply::new(id, response))
                            .await;
                    }
                    Err(error) => reply_error(&connection, id, error).await,
                }
            });
        }
        METHOD_FS_READ_TEXT_FILE => {
            let Ok(request) = serde_json::from_value::<FsReadTextFileRequest>(params) else {
                reply_error(connection, id, JsonRpcError::method_not_found(method)).await;
                return;
            };
            let connection = Arc::clone(connection);
            let handler = Arc::clone(handler);
            tokio::spawn(async move {
                match handler.read_text_file(request).await {
                    Ok(response) => {
                        let _ = connection
                            .write_frame(&JsonRpcResultReply::new(id, response))
                            .await;
                    }
                    Err(error) => reply_error(&connection, id, error).await,
                }
            });
        }
        METHOD_FS_WRITE_TEXT_FILE => {
            let Ok(request) = serde_json::from_value::<FsWriteTextFileRequest>(params) else {
                reply_error(connection, id, JsonRpcError::method_not_found(method)).await;
                return;
            };
            let connection = Arc::clone(connection);
            let handler = Arc::clone(handler);
            tokio::spawn(async move {
                match handler.write_text_file(request).await {
                    Ok(()) => {
                        let _ = connection
                            .write_frame(&JsonRpcResultReply::new(id, serde_json::Value::Null))
                            .await;
                    }
                    Err(error) => reply_error(&connection, id, error).await,
                }
            });
        }
        // Every terminal method, and anything else the agent invents, is
        // refused: `ClientCapabilities` declares no terminal support and
        // Haider must not advertise what it cannot enforce.
        _ => reply_error(connection, id, JsonRpcError::method_not_found(method)).await,
    }
}

async fn reply_error(connection: &Arc<AcpConnection>, id: JsonRpcId, error: JsonRpcError) {
    let _ = connection
        .write_frame(&JsonRpcErrorReply::new(id, error))
        .await;
}

/// True when a JSON-RPC error in answer to `session/prompt` is a cancellation
/// OUTCOME rather than a failure.
pub fn rpc_error_is_cancellation(error: &JsonRpcError) -> bool {
    error.code == ACP_ERROR_REQUEST_CANCELLED
}
