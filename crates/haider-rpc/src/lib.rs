//! Transport-independent wire contracts for the Haider daemon.
//!
//! [`WireFrame`] is an explicit, versioned protocol union. It is not JSON-RPC:
//! WebSockets carry one JSON object per text message, while Unix-domain sockets
//! carry the same JSON bytes after a four-byte big-endian length prefix.
//!
//! # The sequence-cursor law
//!
//! [`haider_protocol::envelope::RawEnvelope::seq`] is the only replay cursor.
//! Event frames intentionally have no event ID, notification counter, snapshot
//! generation, or other competing resume position. A client resumes after the
//! greatest sequence it has fully applied.
//!
//! # Boundary
//!
//! This crate contains wire shapes, framing codecs, and handshake negotiation
//! only. It owns no listeners, connection/session policy, authorization,
//! persistence, or daemon lifecycle orchestration. `haider-daemon` builds those
//! semantics on this crate.

mod codec;
mod frame;
mod negotiation;

pub mod uds_codec;
pub mod ws_codec;

pub use codec::CodecError;
pub use frame::{
    AttachMode, AttachState, AttachmentId, Capability, CapabilitySet, ClientKind, CommandId,
    DEFAULT_FRAME_LIMIT, ERROR_CODE_ALREADY_RESOLVED, ERROR_CODE_CAPABILITY_DENIED,
    ERROR_CODE_CURSOR_AHEAD, ERROR_CODE_DRAINING, ERROR_CODE_NOT_FOUND, Hello, LifecyclePhase,
    MenuInput, ProtocolError, RequestBody, RequestId, ResponseBody, SeqRange, SessionReadResult,
    SessionSummary, WIRE_PROTOCOL_VERSION, Welcome, WireFrame,
};
pub use negotiation::{Negotiated, ServerRange, negotiate};

/// Crate marker used by the workspace self-test.
pub const CRATE_NAME: &str = "haider-rpc";
