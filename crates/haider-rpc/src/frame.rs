//! Logical wire-frame types.

use std::collections::BTreeSet;

use haider_protocol::DeliveryMode;
use haider_protocol::context::ContextFootprint;
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::ids::{ItemId, MenuId, RunId, SessionId};
use haider_protocol::session::{SessionMetadataV1, SessionPermissionOverridesV1};
use haider_protocol::tool::{AttachmentBlock, ToolInventorySnapshot};
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
string_id!(
    /// One daemon-instance-scoped OAuth browser flow.
    OAuthFlowId
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
/// Stable code for an opaque pagination cursor that cannot be decoded.
pub const ERROR_CODE_INVALID_CURSOR: &str = "invalid_cursor";
/// Stable code for a structurally invalid request: an unknown method or
/// attachment mode, a bad range/limit, or menu coordinates that do not match
/// the committed menu version.
pub const ERROR_CODE_INVALID_ARGUMENT: &str = "invalid_argument";
/// Stable code for a control command fenced by a newer worker generation.
pub const ERROR_CODE_STALE_GENERATION: &str = "stale_generation";
/// Stable code for a command that requires an active/nonterminal run.
pub const ERROR_CODE_RUN_NOT_ACTIVE: &str = "run_not_active";
/// Stable code for a session resource that is already occupied.
///
/// RESERVED in W3c1: golden-pinned per the report's R7 taxonomy but not yet
/// emitted — the daemon currently reports admission pressure (including
/// domain `Busy`) as the retryable [`ERROR_CODE_OVERLOADED`] family. The
/// W3c2 account actor is the intended first emitter; the review round owns
/// the busy-vs-overloaded mapping decision.
pub const ERROR_CODE_BUSY: &str = "busy";
/// Stable code for a provider-side turn failure.
///
/// First emitted by W3c2 login validation (R7): a retryable 429/529/5xx or
/// transport failure during credential validation reports this family with
/// `retryable: true`. Durable turn failures still surface as `RunFailed`
/// envelopes, not correlated responses (R3).
pub const ERROR_CODE_PROVIDER_ERROR: &str = "provider_error";
/// Stable code for a credential that failed authentication (HTTP 401):
/// the key is invalid. Non-retryable.
pub const ERROR_CODE_UNAUTHORIZED: &str = "unauthorized";
/// Stable code for an authenticated identity that lacks permission for the
/// selected model/endpoint (HTTP 403). Non-retryable.
pub const ERROR_CODE_PERMISSION_DENIED: &str = "permission_denied";
/// Stable code for an operation that needs a credential no account provides.
pub const ERROR_CODE_CREDENTIAL_MISSING: &str = "credential_missing";
/// Stable code for a platform without a working secret vault (R10: the W3c
/// vault gate is macOS; non-macOS rejects login before staging/validation
/// with this code, never a generic internal message).
pub const ERROR_CODE_VAULT_UNSUPPORTED: &str = "vault_unsupported";
/// Stable code for a login retry whose staged secret no longer exists
/// (stage/pending-command TTL expiry, disconnect, or daemon restart): the
/// client must stage the secret again — an explicit recovery action, and
/// retryable once re-staged.
pub const ERROR_CODE_RESTAGE_REQUIRED: &str = "restage_required";
/// Stable code for a provider whose sanctioned OAuth registration is absent.
pub const ERROR_CODE_OAUTH_UNAVAILABLE: &str = "oauth_unavailable";
/// Stable code for an absent, expired, or differently-bound OAuth flow/ref.
pub const ERROR_CODE_OAUTH_FLOW_NOT_FOUND: &str = "oauth_flow_not_found";
/// Stable code for a management mutation fenced by a newer account/provider
/// snapshot. Retrying after refreshing that snapshot is the intended recovery.
pub const ERROR_CODE_REVISION_CONFLICT: &str = "revision_conflict";
/// Stable refusal for `provider.remove` when the named profile is not a
/// removable custom provider or credential descriptors still reference it.
pub const ERROR_CODE_PROVIDER_REMOVE_REFUSED: &str = "provider_remove_refused";
/// Stable rejection for a shell builtin whose durable daemon semantics are
/// deliberately not implemented by this protocol slice.
pub const ERROR_CODE_UNSUPPORTED_SHELL_BUILTIN: &str = "unsupported_shell_builtin";

/// Daemon implements receipt-backed session creation and metadata.
pub const FEATURE_SESSION_MUTATION_V1: &str = "session_mutation_v1";
/// Daemon implements durable submit/cancel turn control.
pub const FEATURE_TURN_CONTROL_V1: &str = "turn_control_v1";
/// Daemon implements durable idle-only context compaction.
pub const FEATURE_CONTEXT_COMPACTION_V1: &str = "context_compaction_v1";
/// Daemon implements the durable `account.login_api` command (R7/R10).
pub const FEATURE_ACCOUNT_LOGIN_API_V1: &str = "account_login_api_v1";
/// Daemon implements connection-scoped `vault.stage` secret staging (R7).
pub const FEATURE_VAULT_STAGE_V1: &str = "vault_stage_v1";
/// Daemon implements loopback authorization-code/PKCE account flows.
pub const FEATURE_ACCOUNT_OAUTH_PKCE_V1: &str = "account_oauth_pkce_v1";
/// Daemon imports OAuth credentials from approved, daemon-local CLI stores.
pub const FEATURE_ACCOUNT_OAUTH_IMPORT_V1: &str = "account_oauth_import_v1";
/// Daemon implements durable `account.add` for an OAuth-ready reference.
pub const FEATURE_ACCOUNT_MANAGEMENT_V1: &str = "account_management_v1";
/// Daemon implements provider management reads.
pub const FEATURE_PROVIDER_MANAGEMENT_V1: &str = "provider_management_v1";
/// Daemon implements durable `provider.configure`.
pub const FEATURE_PROVIDER_CONFIGURE_V1: &str = "provider_configure_v1";
/// Daemon implements durable custom-provider removal.
pub const FEATURE_PROVIDER_REMOVE_V1: &str = "provider_remove_v1";
/// Daemon implements provider-owned model discovery refresh.
pub const FEATURE_PROVIDER_MODELS_V1: &str = "provider_models_v1";
/// Daemon implements live same-provider account rotation.
pub const FEATURE_ACCOUNT_ROTATION_V1: &str = "account_rotation_v1";
/// Daemon implements receipt-backed direct user shell execution.
pub const FEATURE_SHELL_EXEC_V1: &str = "shell_exec_v1";
/// Daemon implements the canonical read-only tool inventory snapshot.
pub const FEATURE_TOOL_INVENTORY_V1: &str = "tool_inventory_v1";
/// Daemon persists and applies typed per-session write/exec permission overrides.
pub const FEATURE_SESSION_PERMISSION_OVERRIDES_V1: &str = "session_permission_overrides_v1";

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
    /// Additive method families implemented by this daemon.
    ///
    /// Capabilities answer whether this connection may control the daemon;
    /// features answer whether the negotiated v1 peer implements a method.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub features: BTreeSet<String>,
}

/// A raw secret in transit on the sensitive same-UID UDS staging path (R7).
///
/// This type exists ONLY in the transport crate — domain `haider-protocol`
/// stays secret-free — and only inside [`RequestBody::VaultStage`], which the
/// daemon serves exclusively on an authenticated same-UID local UDS
/// connection. Laws:
///
/// - `Debug` is unconditionally redacted; ordinary frame formatting can
///   never reveal the value (test-pinned).
/// - The value is zeroized on drop, and both peers zeroize the encoded
///   frame buffers around it (`uds_codec::encode_zeroizing`, the daemon's
///   zeroizing decoder, the client's zeroizing writer).
/// - It must never be converted through a loggable `serde_json::Value`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretWire(String);

impl SecretWire {
    /// Wraps a raw secret for staging.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Grants access to the raw secret bytes; callers copy into their own
    /// zeroizing storage and drop this frame promptly.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }

    /// Whether the staged secret is empty (invalid to stage).
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for SecretWire {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SecretWire([REDACTED])")
    }
}

impl Drop for SecretWire {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.0.zeroize();
    }
}

/// A transient authorization URL whose query and state are secret-bearing.
///
/// This is intentionally not a `String`. Its normal formatting is redacted,
/// its allocation is zeroized on drop, and renderers must use the separately
/// returned provider origin + loopback port.
#[derive(Clone, PartialEq, Eq)]
pub struct OAuthAuthorizationWire {
    value: zeroize::Zeroizing<String>,
    provider_origin: String,
    loopback_port: Option<u16>,
}

impl OAuthAuthorizationWire {
    pub fn new(value: impl Into<String>) -> Self {
        Self::from_zeroizing(zeroize::Zeroizing::new(value.into()))
    }

    /// Moves an already-protected authorization URL into the wire value
    /// without creating a second ordinary secret-bearing allocation.
    pub fn from_zeroizing(value: zeroize::Zeroizing<String>) -> Self {
        let (provider_origin, loopback_port) = safe_authorization_display(&value);
        Self {
            value,
            provider_origin,
            loopback_port,
        }
    }

    /// Grants the browser-link boundary a short-lived view of the full URL.
    pub fn expose_authorization_url(&self) -> &str {
        self.value.as_str()
    }
}

impl std::fmt::Debug for OAuthAuthorizationWire {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OAuthAuthorizationWire")
            .field("provider_origin", &self.provider_origin)
            .field("loopback_port", &self.loopback_port)
            .finish()
    }
}

fn safe_authorization_display(value: &str) -> (String, Option<u16>) {
    let provider_origin = value
        .find("://")
        .and_then(|scheme_end| {
            let authority_start = scheme_end.checked_add(3)?;
            let authority_end = value[authority_start..]
                .find(['/', '?', '#'])
                .map_or(value.len(), |offset| authority_start + offset);
            (authority_end <= 512).then(|| value[..authority_end].to_owned())
        })
        .unwrap_or_else(|| "[REDACTED]".into());
    let redirect = value
        .find("redirect_uri=")
        .map(|start| &value[start + "redirect_uri=".len()..])
        .unwrap_or("");
    let marker = find_ascii_case_insensitive(redirect.as_bytes(), b"127.0.0.1%3a")
        .map(|index| index + b"127.0.0.1%3a".len())
        .or_else(|| {
            redirect
                .find("127.0.0.1:")
                .map(|index| index + "127.0.0.1:".len())
        });
    let loopback_port = marker.and_then(|start| {
        let digits = redirect[start..]
            .bytes()
            .take_while(u8::is_ascii_digit)
            .collect::<Vec<_>>();
        std::str::from_utf8(&digits).ok()?.parse::<u16>().ok()
    });
    (provider_origin, loopback_port)
}

fn find_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle))
}

impl Serialize for OAuthAuthorizationWire {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.value.as_str())
    }
}

impl<'de> Deserialize<'de> for OAuthAuthorizationWire {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self::new)
    }
}

/// Single-use daemon-local claim reference for a verified token bundle.
#[derive(Clone, PartialEq, Eq)]
pub struct OAuthReadyRefWire(zeroize::Zeroizing<String>);

impl OAuthReadyRefWire {
    pub fn new(value: impl Into<String>) -> Self {
        Self(zeroize::Zeroizing::new(value.into()))
    }

    pub fn expose_reference(&self) -> &str {
        self.0.as_str()
    }
}

impl std::fmt::Debug for OAuthReadyRefWire {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("OAuthReadyRefWire([REDACTED])")
    }
}

impl Serialize for OAuthReadyRefWire {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.0.as_str())
    }
}

impl<'de> Deserialize<'de> for OAuthReadyRefWire {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self::new)
    }
}

/// Structured provider OAuth availability. An unavailable method always
/// carries a precise public reason and never allocates a listener.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthAvailabilityWire {
    pub available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Provider adapter family. Unlike the frozen account enums, this enum is
/// tolerant from its first release so an older client can still display a
/// provider introduced by a newer daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ProviderApiFamilyWire {
    AnthropicMessages,
    #[serde(rename = "openai_responses")]
    OpenAiResponses,
    #[serde(rename = "openai_chat_completions")]
    OpenAiChatCompletions,
    #[serde(other)]
    Unknown,
}

/// Immutable authentication requirement of a provider profile.
///
/// Custom providers may use API-key bearer authentication or no
/// authentication. OAuth is release-owned metadata and cannot be created by
/// an arbitrary endpoint configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ProviderAuthRequirementWire {
    ApiKey,
    OAuth,
    None,
    #[serde(other)]
    Unknown,
}

/// Whether a configured provider is currently available for new work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ProviderAvailabilityWire {
    Available,
    Unavailable,
    #[serde(other)]
    Unknown,
}

/// Provider-declared metadata for one pickable model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelDetailWire {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
}

/// One provider's read-only management projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSummaryWire {
    pub provider: String,
    pub api_family: ProviderApiFamilyWire,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub model_details: Vec<ModelDetailWire>,
    #[serde(default)]
    pub auth_methods: Vec<haider_protocol::credential::AuthMethod>,
    pub availability: ProviderAvailabilityWire,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub availability_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    pub enabled: bool,
}

/// Active account coordinate published beside `account.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderActiveWire {
    pub provider: String,
    pub alias: haider_protocol::ids::CredentialAlias,
}

/// Provider default-model coordinate published beside `account.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderDefaultWire {
    pub provider: String,
    pub model: String,
}

/// Public-only flow progress. No variant can carry callback/token secrets or
/// a raw endpoint error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
#[non_exhaustive]
pub enum OAuthFlowStatusWire {
    WaitingBrowser,
    Exchanging,
    Ready {
        oauth_reference: OAuthReadyRefWire,
        identity: String,
        expires_at_ms: u64,
    },
    Failed {
        public_code: String,
    },
    Expired,
    Cancelled,
    #[serde(other)]
    Unknown,
}

/// Tolerant method tag for `account.add`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AccountAddMethod {
    #[serde(rename = "oauth")]
    OAuth,
    #[serde(other)]
    Unknown,
}

/// Why a secret is being staged (R7): the daemon validates the reference is
/// consumed by a matching operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StagePurpose {
    /// A provider API key headed for `account.login_api`.
    ApiKey,
    /// A provider-requested menu secret (`MenuInput::SecretVaultReference`).
    MenuSecret,
    /// Decode artifact for a purpose this crate does not know (tolerance
    /// discipline).
    #[serde(other)]
    Unknown,
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
    /// Additive R2 field: typed configuration for live-created sessions.
    /// `None` for legacy `{}` rows and when an old daemon omits the field —
    /// readers must not infer anything from its absence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<SessionMetadataV1>,
}

/// Result of a non-subscribing session read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionReadResult {
    pub session_id: SessionId,
    pub range: SeqRange,
    pub head_seq: u64,
    /// Additive R2 field; same absence semantics as
    /// [`SessionSummary::metadata`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<SessionMetadataV1>,
    /// Latest durable request-local context snapshot at or before `head_seq`,
    /// independent of the requested envelope range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_context_footprint: Option<ContextFootprint>,
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
    /// Additive source-compatible form of `session.create`. The legacy Rust
    /// variant below remains serializable for existing callers, while wire
    /// decoding normalizes both old and new JSON into this variant.
    #[serde(rename = "session.create")]
    SessionCreateWithPermissionOverrides {
        command_id: CommandId,
        cwd: String,
        provider: String,
        model: String,
        max_tokens: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        permission_overrides: Option<SessionPermissionOverridesV1>,
    },
    /// Atomically creates typed session configuration, a `Created` event, and
    /// the durable command receipt that makes response-loss retries safe.
    ///
    /// This encode-only compatibility variant keeps existing Rust callers
    /// source-compatible. Decoders produce
    /// [`Self::SessionCreateWithPermissionOverrides`] with `None`.
    #[serde(rename = "session.create", skip_deserializing)]
    SessionCreate {
        command_id: CommandId,
        cwd: String,
        provider: String,
        model: String,
        max_tokens: u64,
    },
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
    /// Durably accepts one turn before any provider work begins.
    #[serde(rename = "turn.submit")]
    TurnSubmit {
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        text: String,
        #[serde(default)]
        attachments: Vec<AttachmentBlock>,
        mode: DeliveryMode,
    },
    /// Durably records cancellation intent before waking the worker.
    #[serde(rename = "turn.cancel")]
    TurnCancel {
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        run_id: RunId,
    },
    /// Runs one receipt-backed, idle-only immutable history compaction.
    #[serde(rename = "session.compact")]
    SessionCompact {
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
    },
    /// Executes exact user-supplied shell program bytes on the session daemon.
    /// The command creates no user message and no provider request. `cwd`, when
    /// present, is workspace-relative and applies only to this invocation.
    #[serde(rename = "shell.exec")]
    ShellExec {
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
    /// Reads canonical registered manifests/defaults plus grants reconstructed
    /// from the target session's durable journal.
    #[serde(rename = "tools.inventory")]
    ToolsInventory { session_id: SessionId },
    /// Stages a raw secret in connection-scoped daemon memory and returns an
    /// opaque single-use reference (R7). Intentionally NON-durable: no
    /// command receipt may ever contain a secret. `stage_id` is an ephemeral
    /// client nonce for same-connection retry dedupe only: the same id with
    /// the same bytes returns the same reference; the same id with
    /// different bytes is invalid. Served only on authenticated same-UID
    /// local UDS connections with connection-level Control.
    #[serde(rename = "vault.stage")]
    VaultStage {
        stage_id: String,
        purpose: StagePurpose,
        secret: SecretWire,
    },
    /// Durable API-key login (R10): claims a staged secret, validates it,
    /// commits Keychain + descriptor recoverably, and answers with the
    /// descriptor. Command identity covers provider/resolved-model/alias and
    /// deliberately EXCLUDES the ephemeral `vault_reference`, so a
    /// lost-response retry may supply a freshly staged reference under the
    /// same command id and still recover the original committed result.
    /// `validation_model: None` means the release-owned full model ID in
    /// the resolved profile.
    #[serde(rename = "account.login_api")]
    AccountLoginApi {
        command_id: CommandId,
        provider: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        alias: Option<String>,
        vault_reference: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        validation_model: Option<String>,
    },
    /// Starts a daemon-owned loopback authorization flow. The response is
    /// delivered asynchronously after the coordinator binds `127.0.0.1:0`;
    /// the connection task performs only authorization and bounded handoff.
    #[serde(rename = "account.oauth_start")]
    AccountOAuthStart {
        provider: String,
        desired_alias: String,
        attempt_id: String,
    },
    /// Reads only the public phase of a connection-bound flow.
    #[serde(rename = "account.oauth_status")]
    AccountOAuthStatus {
        flow_id: OAuthFlowId,
        attempt_id: String,
    },
    /// Idempotently cancels a connection-bound flow.
    #[serde(rename = "account.oauth_cancel")]
    AccountOAuthCancel {
        flow_id: OAuthFlowId,
        attempt_id: String,
    },
    /// Imports a sanctioned OAuth bundle from a daemon-local CLI credential
    /// store. Only the source name crosses the wire; token material is read
    /// and retained by the daemon.
    #[serde(rename = "account.oauth_import")]
    AccountOAuthImport {
        command_id: CommandId,
        source: String,
    },
    /// Durable OAuth account creation. `oauth_reference` is transient,
    /// daemon-instance/connection-bound, single-use, and excluded from the
    /// semantic command digest.
    #[serde(rename = "account.add")]
    AccountAdd {
        command_id: CommandId,
        provider: String,
        alias: String,
        auth_method: AccountAddMethod,
        flow_id: OAuthFlowId,
        attempt_id: String,
        oauth_reference: OAuthReadyRefWire,
    },
    /// Durably selects the globally named account. The provider is
    /// intentionally absent: the daemon derives it from descriptor truth.
    #[serde(rename = "account.set_active")]
    AccountSetActive {
        command_id: CommandId,
        alias: String,
    },
    /// Durably removes one globally named account.
    #[serde(rename = "account.remove")]
    AccountRemove {
        command_id: CommandId,
        alias: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_revision: Option<u64>,
    },
    /// Changes only a registered provider's default model.
    #[serde(rename = "account.set_default_model")]
    AccountSetDefaultModel {
        command_id: CommandId,
        provider: String,
        model: String,
        expected_revision: u64,
    },
    /// Lists credential descriptors (View); never secrets.
    #[serde(rename = "account.list")]
    AccountList {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
    },
    /// Lists provider management summaries from the daemon's published
    /// snapshot. This read never probes an endpoint inline.
    #[serde(rename = "provider.list")]
    ProviderList {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
    },
    /// Refreshes one OAuth provider's model inventory from the provider's own
    /// authenticated catalog.
    #[serde(rename = "provider.models_refresh")]
    ProviderModelsRefresh { provider: String },
    /// Creates a custom provider or safely updates mutable fields on an
    /// existing profile. Identity fields are required on create and may be
    /// omitted on update; when supplied for an existing profile they must
    /// match exactly.
    #[serde(rename = "provider.configure")]
    ProviderConfigure {
        command_id: CommandId,
        provider: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        api_family: Option<ProviderApiFamilyWire>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auth_requirement: Option<ProviderAuthRequirementWire>,
        enabled: bool,
        #[serde(default)]
        models: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default_model: Option<String>,
        expected_revision: u64,
    },
    /// Durably removes one custom provider. Release-owned providers and
    /// providers referenced by any credential descriptor are refused.
    #[serde(rename = "provider.remove")]
    ProviderRemove {
        command_id: CommandId,
        provider: String,
        expected_revision: u64,
    },
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
    /// Durable acceptance coordinates of an atomic `session.create` (R2):
    /// a same-command retry receives this exact body from its receipt.
    #[serde(rename = "session.create")]
    SessionCreate {
        session_id: SessionId,
        created_seq: u64,
        worker_generation: u64,
        metadata: SessionMetadataV1,
    },
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
    /// Durable acceptance coordinates of `turn.submit` (R3): `run_id` and
    /// the `UserMessage` sequence committed by the acceptance transaction.
    /// Socket order relative to that transaction's events is NOT promised —
    /// the durable coordinates, not frame order, close the correlation.
    #[serde(rename = "turn.submit")]
    TurnSubmit {
        session_id: SessionId,
        run_id: RunId,
        accepted_seq: u64,
        worker_generation: u64,
        disposition: SubmitDisposition,
    },
    /// Outcome of durable cancellation intent (R5). `terminal_seq` is
    /// present exactly when `status` is `already_terminal`, naming the
    /// run's committed terminal sequence.
    #[serde(rename = "turn.cancel")]
    TurnCancel {
        session_id: SessionId,
        run_id: RunId,
        status: CancelStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        terminal_seq: Option<u64>,
    },
    #[serde(rename = "session.compact")]
    SessionCompact {
        session_id: SessionId,
        run_id: RunId,
        accepted_seq: u64,
        worker_generation: u64,
    },
    /// Durable acceptance coordinates for one direct shell command. Terminal
    /// status and byte output arrive through the ordinary item event stream.
    #[serde(rename = "shell.exec")]
    ShellExec {
        session_id: SessionId,
        item_id: ItemId,
        accepted_seq: u64,
        worker_generation: u64,
    },
    #[serde(rename = "tools.inventory")]
    ToolsInventory {
        session_id: SessionId,
        inventory: ToolInventorySnapshot,
    },
    /// Opaque staged-secret reference (R7): random, connection- and
    /// daemon-instance-scoped, single-use, and expired at
    /// `expires_at_ms` (absolute Unix ms). Disconnect or drain wipes it.
    #[serde(rename = "vault.stage")]
    VaultStage {
        stage_id: String,
        vault_reference: String,
        expires_at_ms: u64,
    },
    /// Committed login result (R10): the descriptor now active for its
    /// provider. A same-command retry receives this exact body from the
    /// durable receipt. Never carries secret material.
    #[serde(rename = "account.login_api")]
    AccountLoginApi {
        descriptor: haider_protocol::credential::CredentialDescriptor,
    },
    /// Start result. Unavailable registrations return this same structured
    /// shape with no flow/URL and a precise reason.
    #[serde(rename = "account.oauth_start")]
    AccountOAuthStart {
        availability: OAuthAvailabilityWire,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        flow_id: Option<OAuthFlowId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        authorization_url: Option<OAuthAuthorizationWire>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_origin: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        loopback_port: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires_at_ms: Option<u64>,
    },
    #[serde(rename = "account.oauth_status")]
    AccountOAuthStatus {
        flow_id: OAuthFlowId,
        status: OAuthFlowStatusWire,
    },
    #[serde(rename = "account.oauth_cancel")]
    AccountOAuthCancel {
        flow_id: OAuthFlowId,
        status: OAuthFlowStatusWire,
    },
    #[serde(rename = "account.oauth_import")]
    AccountOAuthImport {
        descriptor: haider_protocol::credential::CredentialDescriptor,
        revision: u64,
    },
    #[serde(rename = "account.add")]
    AccountAdd {
        descriptor: haider_protocol::credential::CredentialDescriptor,
    },
    #[serde(rename = "account.set_active")]
    AccountSetActive {
        descriptor: haider_protocol::credential::CredentialDescriptor,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prior_alias: Option<haider_protocol::ids::CredentialAlias>,
        revision: u64,
    },
    #[serde(rename = "account.remove")]
    AccountRemove {
        removed_alias: haider_protocol::ids::CredentialAlias,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replacement_active_alias: Option<haider_protocol::ids::CredentialAlias>,
        revision: u64,
    },
    #[serde(rename = "account.set_default_model")]
    AccountSetDefaultModel {
        provider: ProviderSummaryWire,
        revision: u64,
    },
    /// Credential descriptors (never secrets).
    #[serde(rename = "account.list")]
    AccountList {
        #[serde(default)]
        descriptors: Vec<haider_protocol::credential::CredentialDescriptor>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        revision: Option<u64>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        provider_active: Vec<ProviderActiveWire>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        provider_defaults: Vec<ProviderDefaultWire>,
    },
    /// Provider management summaries and their coherent snapshot revision.
    #[serde(rename = "provider.list")]
    ProviderList {
        #[serde(default)]
        providers: Vec<ProviderSummaryWire>,
        revision: u64,
    },
    #[serde(rename = "provider.models_refresh")]
    ProviderModelsRefresh {
        provider: ProviderSummaryWire,
        revision: u64,
    },
    #[serde(rename = "provider.configure")]
    ProviderConfigure {
        provider: ProviderSummaryWire,
        revision: u64,
    },
    #[serde(rename = "provider.remove")]
    ProviderRemove { provider: String, revision: u64 },
    /// Successful durable menu resolution. The same-command retry receives
    /// the original sequence; a different command receives
    /// [`ERROR_CODE_ALREADY_RESOLVED`] instead.
    #[serde(rename = "menu.answer")]
    MenuAnswer { resolution_seq: u64 },
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

/// Worker disposition returned after durable turn acceptance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SubmitDisposition {
    Started,
    Queued,
    SteerPending,
    #[serde(other)]
    Unknown,
}

/// Result of a durable turn-cancellation command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CancelStatus {
    Accepted,
    AlreadyTerminal,
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
    /// A management compare-and-set request observed a newer snapshot
    /// ([`ERROR_CODE_REVISION_CONFLICT`]).
    RevisionConflict {
        expected_revision: u64,
        current_revision: u64,
    },
    /// The provider did not serve a model catalog to the active credential.
    ProviderModelsUnavailable { provider: String, reason: String },
    /// A custom-provider removal was refused. Blocking credential aliases are
    /// carried as typed data so clients never need to parse the message.
    ProviderRemoveRefused {
        provider: String,
        reason: ProviderRemoveRefusalReasonWire,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        blocking_aliases: Vec<String>,
    },
    /// Decode artifact for a data kind this crate does not know (tolerance
    /// discipline).
    #[serde(other)]
    Unknown,
}

/// Machine-readable reason a `provider.remove` command was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ProviderRemoveRefusalReasonWire {
    NotFound,
    ReleaseOwned,
    BlockingAccounts,
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
    /// Replay for the attachment is complete through `high_water_seq`.
    ///
    /// This frame may REPEAT on the same attachment with strictly increasing
    /// `high_water_seq`: the daemon's internal buffering may transparently
    /// resume an attachment from durable history, replaying the gap and
    /// announcing the new head. Clients treat every occurrence identically —
    /// events deduplicate by `seq` alone (R9/R11) — and must not assume the
    /// first caught-up marker is the last.
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
