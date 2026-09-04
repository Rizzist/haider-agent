//! Agent Client Protocol (ACP) v1 wire types for the exact message set Haider
//! speaks to Google's supervised `antigravity-acp` agent.
//!
//! Every field name here was read out of the published v1 JSON schema and is
//! recorded in `docs/testing/v0.0.970/googleoauth_acp-wire-facts.md`; ACP is camelCase on
//! the wire and explicitly extensible through `_meta`, so every type in this
//! file IGNORES unknown fields instead of rejecting them. `protocolVersion` is
//! an integer (uint16), never a string.
//!
//! `rawInput`/`rawOutput` on a tool call are arbitrary agent-authored JSON.
//! They stay [`serde_json::Value`], [`ToolCall`] and [`ToolCallUpdate`] carry a
//! hand-written [`fmt::Debug`] that elides them, and no caller may log them at
//! info level or above.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// ACP protocol version Haider speaks.
///
/// `ProtocolVersion` is a uint16 bumped only for breaking changes, and version
/// negotiation has NO error path: the agent answers with the newest version it
/// supports and the client must inspect the echoed integer and close the
/// connection when it cannot speak it. A v2 schema already exists (in which
/// `authenticate` becomes `auth_login`), so this is pinned and checked.
pub const ACP_PROTOCOL_VERSION: u16 = 1;

/// The only JSON-RPC version an ACP peer may declare.
pub const JSONRPC_VERSION: &str = "2.0";

pub const METHOD_INITIALIZE: &str = "initialize";
pub const METHOD_AUTHENTICATE: &str = "authenticate";
pub const METHOD_SESSION_NEW: &str = "session/new";
pub const METHOD_SESSION_PROMPT: &str = "session/prompt";
pub const METHOD_SESSION_CANCEL: &str = "session/cancel";
pub const METHOD_SESSION_SET_CONFIG_OPTION: &str = "session/set_config_option";
pub const METHOD_SESSION_UPDATE: &str = "session/update";
pub const METHOD_SESSION_REQUEST_PERMISSION: &str = "session/request_permission";
pub const METHOD_FS_READ_TEXT_FILE: &str = "fs/read_text_file";
pub const METHOD_FS_WRITE_TEXT_FILE: &str = "fs/write_text_file";

/// Agent-side error codes observed from the real 1.1.1 binary.
/// `-32000` is returned verbatim as `"Authentication required"` for a
/// `session/new` issued before a successful `authenticate`.
pub const ACP_ERROR_AUTH_REQUIRED: i64 = -32000;
/// Resource not found, e.g. "Session not found in the current GEMINI_HOME".
pub const ACP_ERROR_RESOURCE_NOT_FOUND: i64 = -32002;
/// Standard JSON-RPC method-not-found. Haider answers every ACP method it
/// does not implement with this code rather than advertising a capability it
/// cannot enforce.
pub const ACP_ERROR_METHOD_NOT_FOUND: i64 = -32601;
/// Request cancelled. In answer to `session/prompt` this is a cancellation
/// OUTCOME, never a failure.
pub const ACP_ERROR_REQUEST_CANCELLED: i64 = -32800;

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 envelope
// ---------------------------------------------------------------------------

/// A JSON-RPC id as it appears on the wire. Haider only ever mints numeric
/// ids; the string form exists so an inbound agent request can be answered
/// with its id echoed back byte-identically.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcId {
    Number(i64),
    Text(String),
}

impl JsonRpcId {
    /// Returns the id as one of Haider's own outbound correlators, or `None`
    /// when the peer echoed something Haider never minted.
    pub fn as_outbound(&self) -> Option<u64> {
        match self {
            Self::Number(value) => u64::try_from(*value).ok(),
            Self::Text(value) => value.parse().ok(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcError {
    pub fn method_not_found(method: &str) -> Self {
        Self {
            code: ACP_ERROR_METHOD_NOT_FOUND,
            message: format!("haider does not implement the ACP method {method}"),
            data: None,
        }
    }
}

impl fmt::Display for JsonRpcError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} (code {})", self.message, self.code)
    }
}

/// One decoded inbound line. Every member is optional because the same struct
/// decodes a response, a notification, and an agent-to-client request; the
/// dispatcher classifies by which members are present.
#[derive(Debug, Clone, Deserialize)]
pub struct InboundFrame {
    #[serde(default)]
    pub jsonrpc: Option<String>,
    #[serde(default)]
    pub id: Option<JsonRpcId>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub params: Option<Value>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<JsonRpcError>,
}

impl InboundFrame {
    /// A peer that declares a JSON-RPC version other than 2.0 is speaking a
    /// protocol Haider did not negotiate. An ABSENT `jsonrpc` is tolerated:
    /// the transport rule the agent must obey is about framing, and rejecting
    /// a frame over a missing envelope field would fail an otherwise legible
    /// stream.
    pub fn declares_supported_version(&self) -> bool {
        self.jsonrpc
            .as_deref()
            .is_none_or(|version| version == JSONRPC_VERSION)
    }
}

#[derive(Debug, Serialize)]
pub struct JsonRpcRequest<'a, T: Serialize> {
    jsonrpc: &'static str,
    id: u64,
    method: &'a str,
    params: T,
}

impl<'a, T: Serialize> JsonRpcRequest<'a, T> {
    pub fn new(id: u64, method: &'a str, params: T) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            method,
            params,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct JsonRpcNotification<'a, T: Serialize> {
    jsonrpc: &'static str,
    method: &'a str,
    params: T,
}

impl<'a, T: Serialize> JsonRpcNotification<'a, T> {
    pub fn new(method: &'a str, params: T) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            method,
            params,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct JsonRpcResultReply<T: Serialize> {
    jsonrpc: &'static str,
    id: JsonRpcId,
    result: T,
}

impl<T: Serialize> JsonRpcResultReply<T> {
    pub fn new(id: JsonRpcId, result: T) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            result,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct JsonRpcErrorReply {
    jsonrpc: &'static str,
    id: JsonRpcId,
    error: JsonRpcError,
}

impl JsonRpcErrorReply {
    pub fn new(id: JsonRpcId, error: JsonRpcError) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            error,
        }
    }
}

// ---------------------------------------------------------------------------
// initialize
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeRequest {
    pub protocol_version: u16,
    pub client_capabilities: ClientCapabilities,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_info: Option<ClientInfo>,
}

/// What Haider tells the agent it can serve. Both filesystem verbs and the
/// terminal family are declared FALSE in this slice because the default
/// inbound handler refuses them: an ACP client must never advertise a
/// capability it cannot enforce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    pub fs: FsCapabilities,
    pub terminal: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FsCapabilities {
    pub read_text_file: bool,
    pub write_text_file: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResponse {
    pub protocol_version: u16,
    #[serde(default)]
    pub agent_capabilities: Option<AgentCapabilities>,
    #[serde(default)]
    pub auth_methods: Vec<AuthMethod>,
    #[serde(default)]
    pub agent_info: Option<AgentInfo>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapabilities {
    #[serde(default)]
    pub load_session: bool,
    #[serde(default)]
    pub prompt_capabilities: PromptCapabilities,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptCapabilities {
    #[serde(default)]
    pub image: bool,
    #[serde(default)]
    pub audio: bool,
    #[serde(default)]
    pub embedded_context: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentInfo {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
}

/// `AuthMethod` is a discriminated union on `type`, and the schema documents
/// that an ABSENT `type` means [`AuthMethodType::Agent`]. Google's 1.1.1
/// binary omits the field on all four of its methods, so the default is the
/// live case rather than a corner one.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthMethod {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "type")]
    pub method_type: AuthMethodType,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethodType {
    /// The method id is passed to `authenticate`.
    #[default]
    Agent,
    /// "Client runs the configured agent program as a separate interactive
    /// process, WITHOUT passing this method to `authenticate`."
    Terminal,
    #[serde(other)]
    Other,
}

// ---------------------------------------------------------------------------
// authenticate
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthenticateRequest {
    pub method_id: String,
}

// ---------------------------------------------------------------------------
// session/new
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionRequest {
    pub cwd: String,
    /// Required by the schema even when empty. Haider brokers no MCP servers
    /// into the supervised agent in this slice.
    pub mcp_servers: Vec<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub additional_directories: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionResponse {
    pub session_id: String,
    /// Session MODES. A different thing from the model catalog: see
    /// [`SessionModeState`].
    #[serde(default)]
    pub modes: Option<SessionModeState>,
    /// Session configuration options. The MODEL CATALOG is one of these —
    /// there is no `models` field and no `session/set_model` method in any
    /// published ACP schema, so [`AcpModelCatalog::resolve`] is how a model
    /// list is obtained at all.
    #[serde(default)]
    pub config_options: Vec<SessionConfigOption>,
}

// ---------------------------------------------------------------------------
// Session modes — NOT models
// ---------------------------------------------------------------------------

/// `SessionModeState`: the session's operating MODES.
///
/// `availableModes`/`currentModeId` are the two fields most easily mistaken
/// for a model catalog, and they are not one — modes are switched with
/// `session/set_mode`, and nothing decoded here ever reaches
/// [`AcpModelCatalog`]. Haider does not drive modes in this slice; the block
/// is decoded so it is visibly a different thing rather than an unknown one.
///
/// Every field is optional even where the schema requires it, because a
/// partial or future-shaped `modes` block must not fail the whole
/// [`NewSessionResponse`] — that would take the model catalog down with a
/// value Haider does not even use.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionModeState {
    #[serde(default)]
    pub current_mode_id: Option<String>,
    #[serde(default)]
    pub available_modes: Vec<SessionMode>,
}

/// One entry of [`SessionModeState::available_modes`]. Decode-only.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SessionMode {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

// ---------------------------------------------------------------------------
// Session configuration options — where the MODEL CATALOG lives
// ---------------------------------------------------------------------------

/// The option id Haider falls back to when no option declares the reserved
/// `model` category.
///
/// This is an observed AGENT CONVENTION, not a spec guarantee, which is why it
/// is only ever consulted after the category.
pub const ACP_MODEL_CONFIG_OPTION_ID: &str = "model";

/// One `SessionConfigOption`.
///
/// The schema models this as a `oneOf` discriminated by `type` — exactly the
/// shape [`AuthMethod`] already has in this file — so it is decoded the same
/// way: the shared fields are named, and the discriminator is a scalar enum
/// with a catch-all. An option whose `type` is absent or unknown therefore
/// stays a decodable option that is merely not readable as something it never
/// declared itself to be, instead of a decode failure that would drop the
/// whole catalog with it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfigOption {
    /// `SessionConfigId`. This is the `configId` a
    /// [`SetSessionConfigOptionRequest`] must name.
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// A UX HINT only. The schema states it "MUST NOT be required for
    /// correctness" and that clients "MUST handle missing or unknown
    /// categories gracefully", so an absent or unrecognized category decodes
    /// to [`SessionConfigCategory::Other`] and resolution falls back to the
    /// option id rather than failing.
    #[serde(default)]
    pub category: SessionConfigCategory,
    #[serde(default, rename = "type")]
    pub option_type: SessionConfigOptionType,
    /// `select` carries a `SessionConfigValueId` string here; `boolean`
    /// carries a bool. One wire field carries both variants, so it is kept as
    /// raw JSON and a select's value is read through
    /// [`Self::current_select_value`], which answers `None` for every
    /// non-string payload.
    #[serde(default)]
    pub current_value: Option<Value>,
    /// Published only by a `select`. `anyOf`: a FLAT option array or a GROUPED
    /// one.
    #[serde(default)]
    pub options: Option<SessionConfigSelectOptions>,
}

impl SessionConfigOption {
    /// The current value of a SELECT option, or `None` when the option
    /// published a non-string value (a `boolean`'s bool) or none at all.
    pub fn current_select_value(&self) -> Option<&str> {
        self.current_value.as_ref().and_then(Value::as_str)
    }
}

/// `SessionConfigOptionCategory`. The schema reserves `mode`, `model`
/// ("Model selector") and `model_config`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionConfigCategory {
    Mode,
    /// The reserved "Model selector" category. The FIRST thing
    /// [`AcpModelCatalog::resolve`] looks for.
    Model,
    ModelConfig,
    /// Absent, or a category this version does not know. Both are the same
    /// thing to a client the schema forbids from requiring the field.
    #[default]
    #[serde(other)]
    Other,
}

/// The `type` discriminator of a [`SessionConfigOption`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionConfigOptionType {
    Select,
    Boolean,
    /// Absent, or a type this version does not know.
    #[default]
    #[serde(other)]
    Other,
}

impl SessionConfigOptionType {
    /// Whether an option of this type may be READ as one that publishes a
    /// value set.
    ///
    /// [`Self::Other`] is included deliberately: an unknown or absent `type`
    /// alongside a published `options` array is precisely the case the schema
    /// requires a client to degrade through, and reading it as a select is
    /// what degrading means here. Only a declared `boolean` is excluded,
    /// because it has declared a value that is not one of a set.
    pub fn may_publish_options(self) -> bool {
        !matches!(self, Self::Boolean)
    }
}

/// `SessionConfigSelectOptions`: an `anyOf` of a FLAT option array and a
/// GROUPED one. Both shapes are legal, so both are decoded and the grouped
/// form is flattened at the point of use.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum SessionConfigSelectOptions {
    /// A flat array of [`SessionConfigSelectOption`].
    Flat(Vec<SessionConfigSelectOption>),
    /// An array of [`SessionConfigSelectGroup`].
    Grouped(Vec<SessionConfigSelectGroup>),
}

impl SessionConfigSelectOptions {
    /// Every published option in WIRE ORDER, with group structure flattened
    /// away. A group's own name is display-only and selection names a value,
    /// so nothing selection needs is lost by flattening.
    pub fn flattened(&self) -> Vec<&SessionConfigSelectOption> {
        match self {
            Self::Flat(options) => options.iter().collect(),
            Self::Grouped(groups) => groups
                .iter()
                .flat_map(|group| group.options.iter())
                .collect(),
        }
    }
}

/// `SessionConfigSelectOption`.
///
/// `value` is the `SessionConfigValueId` a [`SetSessionConfigOptionRequest`]
/// carries, and it is NEVER defaulted: its required-ness is what lets the
/// untagged `anyOf` above tell a flat option from a group.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SessionConfigSelectOption {
    pub value: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// `SessionConfigSelectGroup`. `group` and `options` are never defaulted for
/// the same reason `value` is not.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SessionConfigSelectGroup {
    pub group: String,
    #[serde(default)]
    pub name: String,
    pub options: Vec<SessionConfigSelectOption>,
}

/// The model catalog resolved out of one session's configuration options.
///
/// ACP publishes no model list of its own — there is no `models` field, no
/// `availableModels`, no `currentModelId` and no `session/set_model` method in
/// any published schema. The catalog IS whichever [`SessionConfigOption`] the
/// agent designated as its model selector, so this projection carries the
/// `configId` needed to change it alongside the ids and names it offers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AcpModelCatalog {
    /// The `configId` `session/set_config_option` must name.
    pub config_id: String,
    /// The value the agent says the session is CURRENTLY on. `None` when the
    /// selector published none, which a selection policy reads as "this agent
    /// designated no default".
    pub current_value: Option<String>,
    /// Every offered model, in wire order.
    pub models: Vec<AcpModelOption>,
}

/// One offered model: the id a selection names, and the name a UI shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpModelOption {
    /// `SessionConfigValueId`. OPAQUE: `gemini-pro-agent` is a live slug that
    /// no `<family>-<tier>` parse survives, so it is never parsed
    /// structurally.
    pub id: String,
    /// Display name, falling back to the id when the agent published none.
    pub name: String,
}

impl AcpModelCatalog {
    /// Resolves the model selector out of a session's configuration options.
    ///
    /// Order: an option carrying the reserved
    /// [`SessionConfigCategory::Model`], else one whose id is
    /// [`ACP_MODEL_CONFIG_OPTION_ID`]. Each candidate is tried in turn, so a
    /// categorized option that publishes nothing usable does not mask a
    /// conventionally named one that does.
    ///
    /// `None` — and only `None` — means the agent published NO model catalog.
    pub fn resolve(options: &[SessionConfigOption]) -> Option<Self> {
        options
            .iter()
            .filter(|option| option.category == SessionConfigCategory::Model)
            .find_map(Self::project)
            .or_else(|| {
                options
                    .iter()
                    .filter(|option| option.id == ACP_MODEL_CONFIG_OPTION_ID)
                    .find_map(Self::project)
            })
    }

    /// Projects ONE option, or `None` when it cannot serve as a model
    /// selector: a declared `boolean`, an option publishing no `options`
    /// array, or one publishing an empty set — a selector offering nothing is
    /// no catalog.
    fn project(option: &SessionConfigOption) -> Option<Self> {
        if !option.option_type.may_publish_options() {
            return None;
        }
        let models: Vec<AcpModelOption> = option
            .options
            .as_ref()?
            .flattened()
            .into_iter()
            .map(|entry| AcpModelOption {
                id: entry.value.clone(),
                name: if entry.name.is_empty() {
                    entry.value.clone()
                } else {
                    entry.name.clone()
                },
            })
            .collect();
        if models.is_empty() {
            return None;
        }
        Some(Self {
            config_id: option.id.clone(),
            current_value: option.current_select_value().map(str::to_owned),
            models,
        })
    }

    /// The offered ids in wire order — the exact shape a selection policy
    /// consumes.
    pub fn model_ids(&self) -> Vec<String> {
        self.models.iter().map(|model| model.id.clone()).collect()
    }
}

/// `SetSessionConfigOptionRequest`.
///
/// The value is a `SessionConfigValueId` STRING, which the schema documents as
/// the DEFAULT variant when `type` is absent on the wire, so no `type` is
/// serialized. Haider never sends the boolean variant: the only configuration
/// option it ever sets is the model selector.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetSessionConfigOptionRequest {
    pub session_id: String,
    pub config_id: String,
    pub value: String,
}

/// `SetSessionConfigOptionResponse`: the FULL option set again, with current
/// values, so a caller refreshes its cache from the agent's answer instead of
/// assuming its write took.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetSessionConfigOptionResponse {
    #[serde(default)]
    pub config_options: Vec<SessionConfigOption>,
}

/// `ConfigOptionUpdate`, carried by the `config_option_update` session update:
/// the FULL option set with current values, never a delta. What it carries
/// REPLACES a cached catalog.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigOptionUpdate {
    #[serde(default)]
    pub config_options: Vec<SessionConfigOption>,
}

// ---------------------------------------------------------------------------
// session/prompt and session/cancel
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptRequest {
    pub session_id: String,
    pub prompt: Vec<ContentBlock>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptResponse {
    pub stop_reason: StopReason,
}

/// The exhaustive v1 `StopReason` enum.
///
/// Deliberately has NO catch-all: an unrecognized terminal outcome cannot be
/// mapped onto a Haider `FinishReason` honestly, so it decodes as a malformed
/// frame and the turn ends with one typed error instead of a wrong outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
}

/// `session/cancel` is a NOTIFICATION: the agent sends no response, and the
/// turn's terminal arrives as the `session/prompt` response instead.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelNotification {
    pub session_id: String,
}

// ---------------------------------------------------------------------------
// session/update
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionNotification {
    pub session_id: String,
    pub update: SessionUpdate,
}

/// The exhaustive v1 `sessionUpdate` variant list plus a catch-all, so an
/// unknown FUTURE variant is ignored rather than failing the stream.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "sessionUpdate", rename_all = "snake_case")]
pub enum SessionUpdate {
    UserMessageChunk(ContentChunk),
    AgentMessageChunk(ContentChunk),
    AgentThoughtChunk(ContentChunk),
    ToolCall(ToolCall),
    ToolCallUpdate(ToolCallUpdate),
    Plan,
    AvailableCommandsUpdate,
    CurrentModeUpdate,
    ConfigOptionUpdate(ConfigOptionUpdate),
    SessionInfoUpdate,
    UsageUpdate(UsageUpdate),
    #[serde(other)]
    Other,
}

/// "A change in `messageId` indicates a new message."
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentChunk {
    pub content: ContentBlock,
    #[serde(default)]
    pub message_id: Option<String>,
}

/// Context-window occupancy, NOT billing and NOT subscription quota: `used` is
/// "tokens currently in context" and `size` is "total context window size in
/// tokens". Antigravity never actually sends this update.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct UsageUpdate {
    #[serde(default)]
    pub used: u64,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub cost: Option<f64>,
}

/// The five v1 `ContentBlock` variants plus a catch-all. `Other` is never
/// produced by Haider; it exists so an unknown inbound block is skipped
/// instead of failing the frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    #[serde(rename_all = "camelCase")]
    Image {
        #[serde(default)]
        data: String,
        #[serde(default)]
        mime_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uri: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Audio {
        #[serde(default)]
        data: String,
        #[serde(default)]
        mime_type: String,
    },
    #[serde(rename_all = "camelCase")]
    ResourceLink {
        #[serde(default)]
        uri: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    Resource {
        #[serde(default)]
        resource: Value,
    },
    #[serde(other)]
    Other,
}

impl ContentBlock {
    /// The block's display text, or `None` for a non-text block. Haider has no
    /// stream event for an inline agent-authored image, so those are elided.
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Text { text } => Some(text.as_str()),
            _ => None,
        }
    }
}

/// One agent-executed tool call. Haider NEVER dispatches these; they are
/// display-only, so only the fields a display row needs are modeled and
/// `locations` is left to the unknown-field path.
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub tool_call_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub kind: ToolKind,
    #[serde(default)]
    pub status: ToolCallStatus,
    #[serde(default)]
    pub content: Vec<ToolCallContent>,
    #[serde(default)]
    pub raw_input: Option<Value>,
    #[serde(default)]
    pub raw_output: Option<Value>,
}

impl fmt::Debug for ToolCall {
    /// Elides `rawInput`/`rawOutput`: they are arbitrary agent-authored JSON
    /// and this type is reachable from a `Debug`-formatted log line.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolCall")
            .field("tool_call_id", &self.tool_call_id)
            .field("kind", &self.kind)
            .field("status", &self.status)
            .field("content_blocks", &self.content.len())
            .field("raw_input", &Elided(self.raw_input.is_some()))
            .field("raw_output", &Elided(self.raw_output.is_some()))
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallUpdate {
    pub tool_call_id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub kind: Option<ToolKind>,
    #[serde(default)]
    pub status: Option<ToolCallStatus>,
    #[serde(default)]
    pub content: Vec<ToolCallContent>,
    #[serde(default)]
    pub raw_input: Option<Value>,
    #[serde(default)]
    pub raw_output: Option<Value>,
}

impl fmt::Debug for ToolCallUpdate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolCallUpdate")
            .field("tool_call_id", &self.tool_call_id)
            .field("kind", &self.kind)
            .field("status", &self.status)
            .field("content_blocks", &self.content.len())
            .field("raw_input", &Elided(self.raw_input.is_some()))
            .field("raw_output", &Elided(self.raw_output.is_some()))
            .finish_non_exhaustive()
    }
}

/// Renders only whether an arbitrary-JSON field was present.
struct Elided(bool);

impl fmt::Debug for Elided {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(if self.0 { "<present>" } else { "<absent>" })
    }
}

/// One entry of a tool call's `content` array.
///
/// The wire-facts document does not enumerate the `ToolCallContent` union, so
/// no variant name is invented here: the entry is decoded tolerantly and only
/// a nested `content` block — the one shape a display preview can use — is
/// lifted out. Every other shape decodes to `None` and is elided.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolCallContent {
    #[serde(default)]
    pub content: Option<ContentBlock>,
}

/// The exhaustive v1 `ToolKind` enum plus a catch-all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    SwitchMode,
    #[default]
    #[serde(other)]
    Other,
}

impl ToolKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Edit => "edit",
            Self::Delete => "delete",
            Self::Move => "move",
            Self::Search => "search",
            Self::Execute => "execute",
            Self::Think => "think",
            Self::Fetch => "fetch",
            Self::SwitchMode => "switch_mode",
            Self::Other => "other",
        }
    }
}

/// Tool-call lifecycle status.
///
/// The wire-facts document pins the `ToolCall` FIELD list but not this enum's
/// value set, so the four values ACP v1 defines are modeled and everything
/// else decodes to [`ToolCallStatus::Other`]. Only [`Self::is_terminal`] is
/// load-bearing: a non-terminal status simply means no result row yet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    #[default]
    Pending,
    InProgress,
    Completed,
    Failed,
    #[serde(other)]
    Other,
}

impl ToolCallStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed)
    }

    pub fn is_error(self) -> bool {
        matches!(self, Self::Failed)
    }
}

// ---------------------------------------------------------------------------
// Client-side (agent -> client) requests Haider answers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestPermissionRequest {
    pub session_id: String,
    #[serde(default)]
    pub tool_call: Value,
    #[serde(default)]
    pub options: Vec<PermissionOption>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOption {
    pub option_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub kind: PermissionOptionKind,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
    #[default]
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RequestPermissionResponse {
    pub outcome: PermissionOutcome,
}

impl RequestPermissionResponse {
    /// The outcome the schema REQUIRES for every still-pending permission
    /// request once the client has sent `session/cancel`.
    pub fn cancelled() -> Self {
        Self {
            outcome: PermissionOutcome::Cancelled,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PermissionOutcome {
    Cancelled,
    #[serde(rename_all = "camelCase")]
    Selected {
        option_id: String,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsReadTextFileRequest {
    pub session_id: String,
    pub path: String,
    #[serde(default)]
    pub line: Option<u32>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FsReadTextFileResponse {
    pub content: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsWriteTextFileRequest {
    pub session_id: String,
    pub path: String,
    pub content: String,
}
