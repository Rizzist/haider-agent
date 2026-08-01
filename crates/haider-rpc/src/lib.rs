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

/// Domain types carried by the RPC frames. Re-exporting the existing public
/// dependency lets transport clients reduce envelopes without adding a second
/// direct workspace dependency.
pub use haider_protocol;

pub use codec::CodecError;
pub use frame::{
    AccountAddMethod, AttachMode, AttachState, AttachmentId, CancelStatus, Capability,
    CapabilitySet, ClientKind, CommandId, DEFAULT_FRAME_LIMIT, ERROR_CODE_ALREADY_RESOLVED,
    ERROR_CODE_BUSY, ERROR_CODE_CAPABILITY_DENIED, ERROR_CODE_CREDENTIAL_MISSING,
    ERROR_CODE_CURSOR_AHEAD, ERROR_CODE_DRAINING, ERROR_CODE_INVALID_ARGUMENT,
    ERROR_CODE_INVALID_CURSOR, ERROR_CODE_NOT_FOUND, ERROR_CODE_OAUTH_FLOW_NOT_FOUND,
    ERROR_CODE_OAUTH_UNAVAILABLE, ERROR_CODE_OVERLOADED, ERROR_CODE_PERMISSION_DENIED,
    ERROR_CODE_PROVIDER_ERROR, ERROR_CODE_PROVIDER_REMOVE_REFUSED, ERROR_CODE_RESTAGE_REQUIRED,
    ERROR_CODE_REVISION_CONFLICT, ERROR_CODE_RUN_NOT_ACTIVE, ERROR_CODE_STALE_GENERATION,
    ERROR_CODE_UNAUTHORIZED, ERROR_CODE_UNSUPPORTED_SHELL_BUILTIN, ERROR_CODE_VAULT_UNSUPPORTED,
    ErrorData, FEATURE_ACCOUNT_LOGIN_API_V1, FEATURE_ACCOUNT_MANAGEMENT_V1,
    FEATURE_ACCOUNT_OAUTH_IMPORT_V1, FEATURE_ACCOUNT_OAUTH_PKCE_V1, FEATURE_ACCOUNT_ROTATION_V1,
    FEATURE_BRANCH_CREATE_V1, FEATURE_CONTEXT_COMPACTION_V1, FEATURE_PROVIDER_CONFIGURE_V1,
    FEATURE_PROVIDER_MANAGEMENT_V1, FEATURE_PROVIDER_MODELS_V1, FEATURE_PROVIDER_REMOVE_V1,
    FEATURE_SESSION_MUTATION_V1, FEATURE_SESSION_PERMISSION_OVERRIDES_V1, FEATURE_SHELL_EXEC_V1,
    FEATURE_TOOL_INVENTORY_V1, FEATURE_TURN_CONTROL_V1, FEATURE_VAULT_STAGE_V1, Hello,
    LifecyclePhase, MenuInput, ModelDetailWire, OAuthAuthorizationWire, OAuthAvailabilityWire,
    OAuthFlowId, OAuthFlowStatusWire, OAuthReadyRefWire, ProtocolError, ProviderActiveWire,
    ProviderApiFamilyWire, ProviderAuthRequirementWire, ProviderAvailabilityWire,
    ProviderDefaultWire, ProviderRemoveRefusalReasonWire, ProviderSummaryWire, RequestBody,
    RequestId, ResponseBody, SecretWire, SeqRange, SessionReadResult, SessionSummary, StagePurpose,
    SubmitDisposition, WIRE_PROTOCOL_VERSION, Welcome, WireFrame,
};
pub use negotiation::{Negotiated, ServerRange, negotiate};

/// Crate marker used by the workspace self-test.
pub const CRATE_NAME: &str = "haider-rpc";
