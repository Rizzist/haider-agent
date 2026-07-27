//! Logical wire-frame types.

use std::collections::BTreeSet;

use haider_protocol::envelope::RawEnvelope;
use haider_protocol::ids::{MenuId, SessionId};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The logical wire protocol encoded by this crate.
///
/// Decoding is deliberately strict about the top-level `"v"` field: any other
/// value is rejected, unlike unknown frame kinds, methods, and object fields,
/// which are tolerated. A version bump is a contract change; silent
/// cross-version decoding is not.
pub const WIRE_PROTOCOL_VERSION: u32 = 1;

/// Default v0.1 JSON body limit: 8 MiB.
///
/// W3b advertises its actual configured value in [`Welcome::frame_limit`].
pub const DEFAULT_FRAME_LIMIT: usize = 8 * 1024 * 1024;

const fn default_frame_limit_u32() -> u32 {
    DEFAULT_FRAME_LIMIT as u32
}

macro_rules! string_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            /// Constructs an opaque identifier.
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            /// Returns the opaque identifier text.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

string_id!(
    /// A connection-scoped, client-generated request identifier.
    RequestId
);
string_id!(
    /// An attachment created by `session.attach`.
    AttachmentId
);
string_id!(
    /// A durable, client-generated idempotency key.
    CommandId
);

/// Stable code for a request whose replay cursor is beyond the committed head.
pub const ERROR_CODE_CURSOR_AHEAD: &str = "cursor_ahead";
/// Stable code for a request forbidden by the connection's granted capabilities.
pub const ERROR_CODE_CAPABILITY_DENIED: &str = "capability_denied";
/// Stable code for a compare-and-set request that lost to an earlier resolution.
pub const ERROR_CODE_ALREADY_RESOLVED: &str = "already_resolved";
/// Stable code for a requested session, attachment, menu, or other resource not found.
pub const ERROR_CODE_NOT_FOUND: &str = "not_found";
/// Stable code for work rejected after the daemon entered its drain barrier.
pub const ERROR_CODE_DRAINING: &str = "draining";
/// Stable code for work refused because a daemon resource limit is already
/// reached — the connection admission cap is the first user (report §2.5).
/// Retrying later, after other work finishes, is the intended recovery.
pub const ERROR_CODE_OVERLOADED: &str = "overloaded";

/// Kind of client taking part in the handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ClientKind {
    Cli,
    Tui,
    Gui,
    Headless,
    #[serde(other)]
    Unknown,
}

/// Connection capability requested or granted during negotiation.
///
/// The wire crate only models the `view | control` set; enforcing what a
/// capability permits is daemon (W3b) authorization policy, never codec logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Capability {
    /// Receive session events.
    View,
    /// Additionally submit control commands such as [`WireFrame::MenuAnswer`].
    Control,
    /// Decode artifact for a capability this crate does not know. It is never
    /// granted by [`crate::negotiate`].
    #[serde(other)]
    Unknown,
}

/// A deterministically encoded set of capabilities.
pub type CapabilitySet = BTreeSet<Capability>;

/// Client handshake parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// Lowest wire protocol version the client implements (inclusive).
    pub protocol_min: u32,
    /// Highest wire protocol version the client implements (inclusive).
    pub protocol_max: u32,
    /// Human-readable client product name, such as `haider-tui`.
    #[serde(default)]
    pub client_name: String,
    /// Client build/version string used for diagnostics and compatibility policy.
    #[serde(default)]
    pub client_version: String,
    /// Random identity for this client process instance.
    #[serde(default)]
    pub client_instance_id: String,
    pub client_kind: ClientKind,
    /// Ceiling for the grant: negotiation returns a subset of this set and
    /// never invents a capability the client did not ask for.
    #[serde(default)]
    pub capabilities_requested: CapabilitySet,
    /// Largest JSON body this client can receive.
    ///
    /// The daemon must not send a frame larger than the smaller of this value
    /// and its own configured limit. The default preserves decode tolerance
    /// for pre-release peers that omitted the additive field.
    #[serde(default = "default_frame_limit_u32")]
    pub max_receive_frame: u32,
}

/// Daemon lifecycle state advertised in [`Welcome`].
///
/// The wire crate only names the phases; their transitions and guarantees are
/// owned by W3b's recovery/drain machinery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LifecyclePhase {
    Starting,
    Recovering,
    Ready,
    Draining,
    Finalizing,
    Stopped,
    Failed,
    #[serde(other)]
    Unknown,
}

/// Server handshake response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    /// The single wire protocol version selected by negotiation.
    pub protocol: u32,
    /// Random process-instance identity. W3b supplies its generation semantics.
    pub instance_id: String,
    /// Durable per-profile daemon generation. This is not a worker generation.
    pub daemon_generation: u64,
    /// Maximum JSON body bytes per frame on either transport. Both peers must
    /// enforce this limit before allocating a body buffer.
    pub frame_limit: u32,
    /// Durable profile identity served by this connection.
    #[serde(default)]
    pub profile_id: String,
    /// Daemon build/version string used for diagnostics and compatibility policy.
    #[serde(default)]
    pub daemon_version: String,
    pub lifecycle_phase: LifecyclePhase,
    /// Granted capability set: a subset of [`Hello::capabilities_requested`].
    #[serde(default)]
    pub capabilities_granted: CapabilitySet,
}

/// Inclusive sequence range for a non-subscribing session read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeqRange {
    /// Inclusive lower bound.
    pub start_seq: u64,
    /// Inclusive upper bound.
    pub end_seq: u64,
}

/// Requested attachment authority; mirrors [`Capability`] per attachment.
///
/// Whether the daemon honors the requested mode is authorization policy owned
/// by W3b, not by this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AttachMode {
    View,
    Control,
    #[serde(other)]
    Unknown,
}

/// Metadata returned when an attachment is established.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachState {
    pub session_id: SessionId,
    /// Echo of the `after_seq` the client attached with: the greatest sequence
    /// it reported as fully applied (zero for complete history).
    pub requested_after_seq: u64,
    /// Committed head captured at attach time. Replay covers
    /// `(requested_after_seq, replay_through_seq]`; higher sequences are live.
    pub replay_through_seq: u64,
    /// Session/execution-scoped generation, distinct from daemon generation.
    pub worker_generation: u64,
    /// W3b fills this with the authority epoch observed at attachment time.
    pub authority_epoch: u64,
}

/// Cheap metadata returned by session listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: SessionId,
    /// Greatest committed envelope sequence for the session.
    pub head_seq: u64,
    pub worker_generation: u64,
}

/// Result of a non-subscribing session read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionReadResult {
    pub session_id: SessionId,
    pub range: SeqRange,
    pub head_seq: u64,
    #[serde(default)]
    pub envelopes: Vec<RawEnvelope>,
}

/// v0.1 request method bodies.
///
/// The internally tagged method object keeps each operation visibly named and
/// avoids JSON-RPC's method/params semantics. Unknown future methods decode to
/// [`RequestBody::Unknown`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method")]
#[non_exhaustive]
pub enum RequestBody {
    /// Cursor-paginated, non-subscribing session listing.
    ///
    /// v0.1 ordering is the immutable `session_id` in ascending byte order.
    /// `cursor` is an opaque server token positioned after the last emitted
    /// ordering key; clients must return it verbatim and never parse it as an
    /// array offset.
    #[serde(rename = "session.list")]
    SessionList {
        /// Omitted for the first page; otherwise the prior response's token.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cursor: Option<String>,
        /// Maximum number of summaries to return.
        limit: u32,
    },
    /// Non-subscribing read of committed envelopes in an inclusive range.
    #[serde(rename = "session.read")]
    SessionRead {
        session_id: SessionId,
        range: SeqRange,
    },
    /// The only operation that begins event delivery. `after_seq` is the
    /// greatest sequence the client has fully applied (zero for complete
    /// history); the daemon replays strictly after it.
    #[serde(rename = "session.attach")]
    SessionAttach {
        session_id: SessionId,
        after_seq: u64,
        mode: AttachMode,
    },
    /// Ends event delivery for one attachment; never affects session
    /// authority or worker ownership.
    #[serde(rename = "session.detach")]
    SessionDetach { attachment_id: AttachmentId },
    /// Decode artifact for a method this crate does not know (tolerance
    /// discipline). W3b answers it with a protocol error, not a panic.
    #[serde(other)]
    Unknown,
}

/// v0.1 response method bodies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method")]
#[non_exhaustive]
pub enum ResponseBody {
    /// One page in the fixed `session_id` ascending order.
    #[serde(rename = "session.list")]
    SessionList {
        #[serde(default)]
        sessions: Vec<SessionSummary>,
        /// Omitted on the last page; otherwise pass verbatim as the next
        /// [`RequestBody::SessionList`] cursor.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        next_cursor: Option<String>,
    },
    #[serde(rename = "session.read")]
    SessionRead { result: SessionReadResult },
    #[serde(rename = "session.attach")]
    SessionAttach {
        attachment_id: AttachmentId,
        attach_state: AttachState,
    },
    #[serde(rename = "session.detach")]
    SessionDetach { attachment_id: AttachmentId },
    /// A request-correlated operation failure.
    ///
    /// Stable v0.1 codes include [`ERROR_CODE_CURSOR_AHEAD`],
    /// [`ERROR_CODE_CAPABILITY_DENIED`], [`ERROR_CODE_ALREADY_RESOLVED`],
    /// [`ERROR_CODE_NOT_FOUND`], [`ERROR_CODE_DRAINING`], and
    /// [`ERROR_CODE_OVERLOADED`]. Unknown future string codes remain carryable
    /// by older clients.
    #[serde(rename = "error")]
    Error {
        /// Stable machine-readable `snake_case` code.
        code: String,
        /// Human-readable detail; never load-bearing for client behavior.
        message: String,
        /// Whether retrying after the stated condition changes may succeed.
        retryable: bool,
        /// Typed recovery coordinates for codes that carry them (report
        /// §5.4/§5.6): [`ERROR_CODE_CURSOR_AHEAD`] and
        /// [`ERROR_CODE_ALREADY_RESOLVED`] MUST attach their variant so a
        /// client can act without parsing `message`. `None` for codes with
        /// nothing structured to say, and on frames from older daemons.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<ErrorData>,
    },
    /// Decode artifact for a method this crate does not know (tolerance
    /// discipline).
    #[serde(other)]
    Unknown,
}

/// Machine-readable recovery coordinates attached to a correlated
/// [`ResponseBody::Error`].
///
/// Tagged by `code`-matching kind so future codes can add variants without
/// breaking old clients; an unknown kind decodes as [`ErrorData::Unknown`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ErrorData {
    /// The client's `after_seq` is beyond the committed head
    /// ([`ERROR_CODE_CURSOR_AHEAD`]): reattach from a sequence at or below
    /// `head`.
    CursorAhead {
        /// The cursor the client asked to resume after.
        requested: u64,
        /// The greatest committed sequence the daemon holds.
        head: u64,
    },
    /// A compare-and-set command lost to an earlier resolution
    /// ([`ERROR_CODE_ALREADY_RESOLVED`]): the winning resolution is the
    /// envelope at `resolution_seq` on the event stream.
    AlreadyResolved {
        /// Sequence of the envelope recording the winning resolution.
        resolution_seq: u64,
    },
    /// Decode artifact for a data kind this crate does not know (tolerance
    /// discipline).
    #[serde(other)]
    Unknown,
}

/// Protocol-error wire shape, also returned by failed negotiation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolError {
    /// Stable, machine-readable `snake_case` token. Codes are strings on the
    /// wire so an old client can carry a code it does not recognize.
    pub code: String,
    /// Human-readable detail; never load-bearing for client behavior.
    pub message: String,
    /// When `true`, the sender will close the connection after this frame.
    pub fatal: bool,
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ProtocolError {}

/// Optional value submitted with a menu selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum MenuInput {
    /// Free-form input for question/file-style menus.
    Text {
        /// User-entered non-secret text.
        text: String,
    },
    /// Reference to a secret previously stored through a non-journaled vault RPC.
    ///
    /// The raw secret must never appear in this wire frame.
    SecretVaultReference {
        /// Opaque vault reference resolvable by the daemon.
        vault_reference: String,
    },
}

/// One versioned logical frame shared by WebSocket and UDS transports.
///
/// # Serde tagging rationale
///
/// The JSON representation is internally tagged with a stable `kind` and a
/// top-level `"v": 1`. Internal tagging was chosen over adjacent tagging
/// because it keeps every frame one flat, inspectable object — the version,
/// the discriminant, and the fields sit side by side, which keeps golden
/// fixtures readable and lets tooling grep a transcript by `"kind"`. Adjacent
/// tagging would bury variant fields under a content key for no wire benefit.
/// Unknown object fields are intentionally ignored by Serde; an unknown
/// `kind` decodes to [`WireFrame::Unknown`] (tolerance discipline), while a
/// wrong `"v"` is rejected outright (see [`WIRE_PROTOCOL_VERSION`]).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum WireFrame {
    /// First application frame, client to daemon. Authentication happens
    /// before this frame (WS) or via endpoint access (UDS), never inside it.
    Hello(Hello),
    /// Daemon reply to [`WireFrame::Hello`]; carries the negotiation outcome.
    Welcome(Welcome),
    /// Correlated operation. `request_id` is connection-scoped: the matching
    /// [`WireFrame::Response`] echoes it. It is not an idempotency key —
    /// retrying across connections requires a durable [`CommandId`].
    Request {
        request_id: RequestId,
        body: RequestBody,
    },
    /// Answer to the [`WireFrame::Request`] whose `request_id` it echoes.
    Response {
        request_id: RequestId,
        body: ResponseBody,
    },
    /// One committed envelope. `envelope.seq` is the ONLY replay cursor:
    /// there is deliberately no frame-level event ID, counter, or snapshot
    /// generation to compete with it. Delivery is at-least-once; clients drop
    /// `seq <= last_applied` and treat a gap as a signal to reattach.
    Event {
        attachment_id: AttachmentId,
        session_id: SessionId,
        envelope: RawEnvelope,
    },
    /// Replay for the attachment is complete through `high_water_seq`; every
    /// later [`WireFrame::Event`] on this attachment is live.
    AttachCaughtUp {
        attachment_id: AttachmentId,
        high_water_seq: u64,
    },
    /// Wire shape of the durable compare-and-set menu command: first
    /// committed answer wins, and `request_seq` plus `worker_generation`
    /// fence stale answers. Only the shape lives here — validation,
    /// arbitration, and the append are daemon (W3b) work.
    MenuAnswer {
        /// Optional connection-scoped correlation for the daemon's answer.
        ///
        /// The durable compare-and-set identity is, and stays, `command_id`;
        /// this field exists only so a CAS loser can be told through a
        /// [`Self::Response`] — which requires a [`RequestId`] — that it lost
        /// ([`ERROR_CODE_ALREADY_RESOLVED`]). A client that omits it accepts
        /// an uncorrelated [`Self::ProtocolError`] instead; older daemons that
        /// never sent the field keep decoding.
        request_id: Option<RequestId>,
        command_id: CommandId,
        session_id: SessionId,
        menu_id: MenuId,
        request_seq: u64,
        worker_generation: u64,
        /// Stable key from the committed menu option.
        option_key: String,
        /// Display-order index from the same committed menu version.
        option_index: u32,
        /// Optional free-form text or secret vault reference.
        input: Option<MenuInput>,
    },
    /// The daemon dropped this attachment under backpressure.
    ///
    /// `last_queued_seq` is informational server telemetry, not resume
    /// authority: queued does not mean fully applied. Under the R9 cursor law,
    /// every client reattaches using its own greatest fully applied sequence.
    Lagged {
        attachment_id: AttachmentId,
        last_queued_seq: u64,
    },
    /// The daemon entered its drain window and will stop accepting new work.
    ServerDraining {
        /// Human-readable/operator-facing drain cause.
        reason: String,
        /// Random identity of the draining daemon process.
        instance_id: String,
        /// Durable per-profile generation of the draining daemon.
        daemon_generation: u64,
        /// Absolute Unix timestamp in milliseconds.
        ///
        /// This is never a duration. At or after this instant the daemon may
        /// force remaining work to stop.
        deadline_unix_ms: u64,
    },
    /// Uncorrelated liveness probe; `nonce` is echoed verbatim by [`Self::Pong`].
    ///
    /// Ping/Pong are top-level frames per the binding protocol report. v0.1
    /// deliberately has no duplicate request-body liveness methods.
    Ping { nonce: u64 },
    /// Top-level answer to [`Self::Ping`].
    Pong { nonce: u64 },
    /// A connection-level fault; `fatal` decides whether the connection closes.
    ///
    /// Request-specific failures use [`ResponseBody::Error`] so they retain
    /// their `request_id` correlation.
    ProtocolError(ProtocolError),
    /// Decode artifact for a frame kind this crate does not know (tolerance
    /// discipline). Never constructed for sending.
    Unknown,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WireFrameRef<'a> {
    Hello(&'a Hello),
    Welcome(&'a Welcome),
    Request {
        request_id: &'a RequestId,
        body: &'a RequestBody,
    },
    Response {
        request_id: &'a RequestId,
        body: &'a ResponseBody,
    },
    Event {
        attachment_id: &'a AttachmentId,
        session_id: &'a SessionId,
        envelope: &'a RawEnvelope,
    },
    AttachCaughtUp {
        attachment_id: &'a AttachmentId,
        high_water_seq: u64,
    },
    MenuAnswer {
        #[serde(skip_serializing_if = "Option::is_none")]
        request_id: &'a Option<RequestId>,
        command_id: &'a CommandId,
        session_id: &'a SessionId,
        menu_id: &'a MenuId,
        request_seq: u64,
        worker_generation: u64,
        option_key: &'a str,
        option_index: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        input: &'a Option<MenuInput>,
    },
    Lagged {
        attachment_id: &'a AttachmentId,
        last_queued_seq: u64,
    },
    ServerDraining {
        reason: &'a str,
        instance_id: &'a str,
        daemon_generation: u64,
        deadline_unix_ms: u64,
    },
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
    ProtocolError(&'a ProtocolError),
    Unknown,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WireFrameOwned {
    Hello(Hello),
    Welcome(Welcome),
    Request {
        request_id: RequestId,
        body: RequestBody,
    },
    Response {
        request_id: RequestId,
        body: ResponseBody,
    },
    Event {
        attachment_id: AttachmentId,
        session_id: SessionId,
        envelope: RawEnvelope,
    },
    AttachCaughtUp {
        attachment_id: AttachmentId,
        high_water_seq: u64,
    },
    MenuAnswer {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<RequestId>,
        command_id: CommandId,
        session_id: SessionId,
        menu_id: MenuId,
        request_seq: u64,
        worker_generation: u64,
        option_key: String,
        option_index: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input: Option<MenuInput>,
    },
    Lagged {
        attachment_id: AttachmentId,
        last_queued_seq: u64,
    },
    ServerDraining {
        reason: String,
        instance_id: String,
        daemon_generation: u64,
        deadline_unix_ms: u64,
    },
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
    ProtocolError(ProtocolError),
    #[serde(other)]
    Unknown,
}

#[derive(Serialize)]
struct VersionedFrameRef<'a> {
    #[serde(rename = "v")]
    version: u32,
    #[serde(flatten)]
    frame: WireFrameRef<'a>,
}

#[derive(Deserialize)]
struct VersionedFrameOwned {
    #[serde(rename = "v")]
    version: u32,
    #[serde(flatten)]
    frame: WireFrameOwned,
}

impl Serialize for WireFrame {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let frame = match self {
            Self::Hello(body) => WireFrameRef::Hello(body),
            Self::Welcome(body) => WireFrameRef::Welcome(body),
            Self::Request { request_id, body } => WireFrameRef::Request { request_id, body },
            Self::Response { request_id, body } => WireFrameRef::Response { request_id, body },
            Self::Event {
                attachment_id,
                session_id,
                envelope,
            } => WireFrameRef::Event {
                attachment_id,
                session_id,
                envelope,
            },
            Self::AttachCaughtUp {
                attachment_id,
                high_water_seq,
            } => WireFrameRef::AttachCaughtUp {
                attachment_id,
                high_water_seq: *high_water_seq,
            },
            Self::MenuAnswer {
                request_id,
                command_id,
                session_id,
                menu_id,
                request_seq,
                worker_generation,
                option_key,
                option_index,
                input,
            } => WireFrameRef::MenuAnswer {
                request_id,
                command_id,
                session_id,
                menu_id,
                request_seq: *request_seq,
                worker_generation: *worker_generation,
                option_key,
                option_index: *option_index,
                input,
            },
            Self::Lagged {
                attachment_id,
                last_queued_seq,
            } => WireFrameRef::Lagged {
                attachment_id,
                last_queued_seq: *last_queued_seq,
            },
            Self::ServerDraining {
                reason,
                instance_id,
                daemon_generation,
                deadline_unix_ms,
            } => WireFrameRef::ServerDraining {
                reason,
                instance_id,
                daemon_generation: *daemon_generation,
                deadline_unix_ms: *deadline_unix_ms,
            },
            Self::Ping { nonce } => WireFrameRef::Ping { nonce: *nonce },
            Self::Pong { nonce } => WireFrameRef::Pong { nonce: *nonce },
            Self::ProtocolError(error) => WireFrameRef::ProtocolError(error),
            Self::Unknown => WireFrameRef::Unknown,
        };
        VersionedFrameRef {
            version: WIRE_PROTOCOL_VERSION,
            frame,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for WireFrame {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let versioned = VersionedFrameOwned::deserialize(deserializer)?;
        if versioned.version != WIRE_PROTOCOL_VERSION {
            return Err(D::Error::custom(format!(
                "unsupported wire version {}; expected {}",
                versioned.version, WIRE_PROTOCOL_VERSION
            )));
        }
        Ok(match versioned.frame {
            WireFrameOwned::Hello(body) => Self::Hello(body),
            WireFrameOwned::Welcome(body) => Self::Welcome(body),
            WireFrameOwned::Request { request_id, body } => Self::Request { request_id, body },
            WireFrameOwned::Response { request_id, body } => Self::Response { request_id, body },
            WireFrameOwned::Event {
                attachment_id,
                session_id,
                envelope,
            } => Self::Event {
                attachment_id,
                session_id,
                envelope,
            },
            WireFrameOwned::AttachCaughtUp {
                attachment_id,
                high_water_seq,
            } => Self::AttachCaughtUp {
                attachment_id,
                high_water_seq,
            },
            WireFrameOwned::MenuAnswer {
                request_id,
                command_id,
                session_id,
                menu_id,
                request_seq,
                worker_generation,
                option_key,
                option_index,
                input,
            } => Self::MenuAnswer {
                request_id,
                command_id,
                session_id,
                menu_id,
                request_seq,
                worker_generation,
                option_key,
                option_index,
                input,
            },
            WireFrameOwned::Lagged {
                attachment_id,
                last_queued_seq,
            } => Self::Lagged {
                attachment_id,
                last_queued_seq,
            },
            WireFrameOwned::ServerDraining {
                reason,
                instance_id,
                daemon_generation,
                deadline_unix_ms,
            } => Self::ServerDraining {
                reason,
                instance_id,
                daemon_generation,
                deadline_unix_ms,
            },
            WireFrameOwned::Ping { nonce } => Self::Ping { nonce },
            WireFrameOwned::Pong { nonce } => Self::Pong { nonce },
            WireFrameOwned::ProtocolError(error) => Self::ProtocolError(error),
            WireFrameOwned::Unknown => Self::Unknown,
        })
    }
}
