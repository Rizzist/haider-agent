//! Logical wire-frame types.

use std::collections::BTreeSet;

use haider_protocol::envelope::RawEnvelope;
use haider_protocol::ids::{MenuId, SessionId};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The logical wire protocol encoded by this crate.
pub const WIRE_PROTOCOL_VERSION: u32 = 1;

/// Default v0.1 JSON body limit: 8 MiB.
///
/// W3b advertises its actual configured value in [`Welcome::frame_limit`].
pub const DEFAULT_FRAME_LIMIT: usize = 8 * 1024 * 1024;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Capability {
    View,
    Control,
    #[serde(other)]
    Unknown,
}

/// A deterministically encoded set of capabilities.
pub type CapabilitySet = BTreeSet<Capability>;

/// Client handshake parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_min: u32,
    pub protocol_max: u32,
    pub client_kind: ClientKind,
    #[serde(default)]
    pub capabilities_requested: CapabilitySet,
}

/// Daemon lifecycle state advertised in [`Welcome`].
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
    pub protocol: u32,
    /// Random process-instance identity. W3b supplies its generation semantics.
    pub instance_id: String,
    /// Durable per-profile daemon generation. This is not a worker generation.
    pub daemon_generation: u64,
    pub frame_limit: u32,
    pub lifecycle_phase: LifecyclePhase,
    #[serde(default)]
    pub capabilities_granted: CapabilitySet,
}

/// Inclusive sequence range for a non-subscribing session read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeqRange {
    pub start_seq: u64,
    pub end_seq: u64,
}

/// Requested attachment authority.
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
    pub requested_after_seq: u64,
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
    #[serde(rename = "session.list")]
    SessionList { page: u64, page_size: u32 },
    #[serde(rename = "session.read")]
    SessionRead {
        session_id: SessionId,
        range: SeqRange,
    },
    #[serde(rename = "session.attach")]
    SessionAttach {
        session_id: SessionId,
        after_seq: u64,
        mode: AttachMode,
    },
    #[serde(rename = "session.detach")]
    SessionDetach { attachment_id: AttachmentId },
    #[serde(rename = "ping")]
    Ping { nonce: u64 },
    #[serde(other)]
    Unknown,
}

/// v0.1 response method bodies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method")]
#[non_exhaustive]
pub enum ResponseBody {
    #[serde(rename = "session.list")]
    SessionList {
        page: u64,
        page_size: u32,
        #[serde(default)]
        sessions: Vec<SessionSummary>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        next_page: Option<u64>,
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
    #[serde(rename = "pong")]
    Pong { nonce: u64 },
    #[serde(other)]
    Unknown,
}

/// Protocol-error wire shape, also returned by failed negotiation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolError {
    pub code: String,
    pub message: String,
    pub fatal: bool,
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ProtocolError {}

/// Server protocol range and capabilities used by [`crate::negotiate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerRange {
    pub protocol_min: u32,
    pub protocol_max: u32,
    pub capabilities: CapabilitySet,
}

/// Successful handshake selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Negotiated {
    pub protocol: u32,
    pub capabilities_granted: CapabilitySet,
}

/// One versioned logical frame shared by WebSocket and UDS transports.
///
/// The JSON representation is internally tagged with a stable `kind` and a
/// top-level `"v": 1`. Internal tagging keeps common fields and variant fields
/// in one inspectable object. Unknown object fields are intentionally ignored
/// by Serde; an unknown `kind` decodes to [`WireFrame::Unknown`].
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum WireFrame {
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
        session_id: SessionId,
        envelope: RawEnvelope,
    },
    AttachCaughtUp {
        attachment_id: AttachmentId,
        high_water_seq: u64,
    },
    MenuAnswer {
        command_id: CommandId,
        session_id: SessionId,
        menu_id: MenuId,
        request_seq: u64,
        worker_generation: u64,
        option: String,
    },
    Lagged {
        attachment_id: AttachmentId,
        resume_after_seq: u64,
    },
    ServerDraining {
        deadline_ms: u64,
    },
    ProtocolError(ProtocolError),
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
        session_id: &'a SessionId,
        envelope: &'a RawEnvelope,
    },
    AttachCaughtUp {
        attachment_id: &'a AttachmentId,
        high_water_seq: u64,
    },
    MenuAnswer {
        command_id: &'a CommandId,
        session_id: &'a SessionId,
        menu_id: &'a MenuId,
        request_seq: u64,
        worker_generation: u64,
        option: &'a str,
    },
    Lagged {
        attachment_id: &'a AttachmentId,
        resume_after_seq: u64,
    },
    ServerDraining {
        deadline_ms: u64,
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
        session_id: SessionId,
        envelope: RawEnvelope,
    },
    AttachCaughtUp {
        attachment_id: AttachmentId,
        high_water_seq: u64,
    },
    MenuAnswer {
        command_id: CommandId,
        session_id: SessionId,
        menu_id: MenuId,
        request_seq: u64,
        worker_generation: u64,
        option: String,
    },
    Lagged {
        attachment_id: AttachmentId,
        resume_after_seq: u64,
    },
    ServerDraining {
        deadline_ms: u64,
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
                session_id,
                envelope,
            } => WireFrameRef::Event {
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
                command_id,
                session_id,
                menu_id,
                request_seq,
                worker_generation,
                option,
            } => WireFrameRef::MenuAnswer {
                command_id,
                session_id,
                menu_id,
                request_seq: *request_seq,
                worker_generation: *worker_generation,
                option,
            },
            Self::Lagged {
                attachment_id,
                resume_after_seq,
            } => WireFrameRef::Lagged {
                attachment_id,
                resume_after_seq: *resume_after_seq,
            },
            Self::ServerDraining { deadline_ms } => WireFrameRef::ServerDraining {
                deadline_ms: *deadline_ms,
            },
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
                session_id,
                envelope,
            } => Self::Event {
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
                command_id,
                session_id,
                menu_id,
                request_seq,
                worker_generation,
                option,
            } => Self::MenuAnswer {
                command_id,
                session_id,
                menu_id,
                request_seq,
                worker_generation,
                option,
            },
            WireFrameOwned::Lagged {
                attachment_id,
                resume_after_seq,
            } => Self::Lagged {
                attachment_id,
                resume_after_seq,
            },
            WireFrameOwned::ServerDraining { deadline_ms } => Self::ServerDraining { deadline_ms },
            WireFrameOwned::ProtocolError(error) => Self::ProtocolError(error),
            WireFrameOwned::Unknown => Self::Unknown,
        })
    }
}
