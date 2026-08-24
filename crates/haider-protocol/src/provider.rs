//! Provider-agnostic model IR (§4): canonical blocks and stream events.
//! Normalize semantics, not quirks; preserve provider-native state as opaque
//! envelopes; capability documents make degradation explicit, never silent.

use crate::ids::{AgentId, CredentialAlias, RunId};
use crate::tool::{AttachmentBlock, ImageBlockRef};
use serde::{Deserialize, Serialize};

/// Stable durable-extension kind used for provider-native continuation state.
pub const PROVIDER_OPAQUE_EXTENSION_KIND: &str = "provider_opaque";

/// Stable turn-item extension kind carrying the bounded web-sources list a
/// turn's citations/grounding produced (W-B). Display-only: provider replay
/// rides the provider-opaque channel, never this item.
pub const WEB_SOURCES_EXTENSION_KIND: &str = "web_sources_v1";

/// One cited/grounded web source surfaced by a provider-executed web tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSource {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// Canonical message content block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "block", rename_all = "snake_case")]
pub enum Block {
    Text {
        text: String,
    },
    Reasoning {
        summary: String,
    },
    ToolCall {
        call_id: String,
        name: String,
        args: serde_json::Value,
    },
    ToolResult {
        call_id: String,
        preview: String,
        truncated: bool,
        /// CAS-backed images attached to this exact tool result. Provider
        /// adapters shape them without moving bytes into the durable message.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageBlockRef>,
    },
    Attachment(AttachmentBlock),
    /// Provider-native continuation state, opaque and provider-keyed —
    /// an Anthropic session continues losslessly on Anthropic (§4).
    ProviderOpaque {
        provider: String,
        data: serde_json::Value,
    },
}

/// Canonical streaming events, normalized across adapters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum StreamEvent {
    TextDelta {
        text: String,
    },
    ReasoningDelta {
        text: String,
    },
    /// Provider refusal content remains semantically distinct from assistant
    /// text. Consumers may display it, but must not replay it as an answer.
    RefusalDelta {
        text: String,
    },
    /// Provider-native continuation state for lossless, same-family replay.
    ProviderOpaque {
        provider: String,
        data: serde_json::Value,
    },
    ToolCallStart {
        call_id: String,
        name: String,
    },
    ToolCallArgsDelta {
        call_id: String,
        args_fragment: String,
    },
    ToolCallEnd {
        call_id: String,
    },
    /// A PROVIDER-executed tool call (Anthropic server tools, OpenAI hosted
    /// web_search, Gemini grounding). Display-only: it never enters the local
    /// dispatch loop, and same-family replay rides `ProviderOpaque`.
    ServerToolUse {
        call_id: String,
        name: String,
        args: serde_json::Value,
    },
    /// The bounded, display-only outcome of one provider-executed tool call.
    ServerToolResult {
        call_id: String,
        preview: String,
        is_error: bool,
    },
    /// Cited/grounded web sources decoded from provider metadata (bounded by
    /// the consumer; display-only).
    WebSources {
        sources: Vec<WebSource>,
    },
    UsageUpdate(Usage),
    Finish {
        reason: FinishReason,
    },
}

/// Cancellation is an OUTCOME, never an error (ACP contract rule): an aborted
/// turn finishes with `Cancelled`, and surfaces must not render it as failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Refusal,
    Cancelled,
    Error,
    /// Anthropic `pause_turn`: the server paused a long-running (server-tool)
    /// turn. The turn engine resends the paused assistant message unchanged;
    /// this reason is never a terminal outcome under the continuation cap.
    PauseTurn,
}

/// Multidimensional, honest token accounting (§4): tagged by source quality
/// and by account so tokenomics never blends subscription with metered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    #[serde(default)]
    pub reasoning: u64,
    #[serde(default)]
    pub cached: u64,
    pub source: UsageSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<CredentialAlias>,
    /// Per-account subtotals when an automatic rotation spans provider
    /// requests in one logical turn. Legacy `account` remains populated only
    /// when exactly one account contributed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accounts: Vec<AccountUsage>,
    /// Provider-neutral accounting. This is additive: the legacy
    /// `input`/`cached` counters above retain their existing wire meaning so
    /// older readers and journal entries continue to decode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub normalized: Option<NormalizedUsage>,
    /// Non-secret cache-domain coordinates attached by the turn engine.
    /// Adapters intentionally leave this absent because they do not own the
    /// session/run/cache-epoch identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<UsageScope>,
    /// Model-registry estimate for input caching. Absent when the model,
    /// cache split, or required write telemetry is unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_cost: Option<CacheCostEstimate>,
    /// One physical provider request's response-local counters and cache
    /// evidence. The legacy fields above remain cumulative within their
    /// run/cache lane; this additive record is what makes individual misses
    /// attributable without changing existing accounting semantics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<RequestUsage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountUsage {
    pub account: CredentialAlias,
    pub input: u64,
    pub output: u64,
    #[serde(default)]
    pub reasoning: u64,
    #[serde(default)]
    pub cached: u64,
    pub source: UsageSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub normalized: Option<NormalizedUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<UsageScope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_cost: Option<CacheCostEstimate>,
}

/// Response-local accounting and cache evidence for one physical provider
/// request. A provider stream may publish several cumulative usage updates;
/// records with the same ordinal replace one another and the last one wins.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestUsage {
    pub ordinal: u64,
    pub input: u64,
    pub output: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached: Option<u64>,
    pub source: UsageSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<CredentialAlias>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub normalized: Option<NormalizedUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_cost: Option<CacheCostEstimate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheRequestDiagnosticV1>,
}

/// Keyed cumulative hashes at the provider-visible cache breakpoints.
///
/// `tools` covers system+tools and `history` covers system+tools+immutable
/// history. This makes the first differing value the location of the first
/// moved prefix component without persisting any prompt bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheBreakpointHashesV1 {
    pub system: String,
    pub tools: String,
    pub history: String,
}

/// The previous request's moving history boundary checked against the
/// current rendered wire. `actual_hash` is absent when the old boundary no
/// longer exists in the current request; absence is never rendered as an
/// all-zero or empty digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviousCacheBreakpointV1 {
    pub message_count: u64,
    pub expected_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_hash: Option<String>,
}

/// Provider-visible breakpoint at which a reused prefix first moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CacheBreakpointV1 {
    System,
    Tools,
    History,
    #[serde(other)]
    Unknown,
}

/// Comparison with the immediately preceding cache entry in this lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
#[non_exhaustive]
pub enum CachePrefixMatchV1 {
    Same,
    Changed {
        first: CacheBreakpointV1,
    },
    Unavailable,
    #[serde(other)]
    Unknown,
}

/// Why an adapter that requires an explicit cache control did not emit one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CacheControlOmissionReasonV1 {
    InvalidBoundaries,
    MissingAccountScope,
    ProviderMismatch,
    UnsupportedModel,
    Unverified,
    AdapterUnavailable,
    #[serde(other)]
    Unknown,
}

/// Exact observation of the cache control in the final synchronous adapter
/// payload. Asynchronous resource adapters use `Unavailable` rather than
/// predicting whether a later network-side create/reuse operation succeeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
#[non_exhaustive]
pub enum CacheControlObservationV1 {
    Emitted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ttl_ms: Option<u64>,
    },
    NotRequired,
    NotEmitted {
        reason: CacheControlOmissionReasonV1,
    },
    Unavailable,
    #[serde(other)]
    Unknown,
}

/// A deliberate cold boundary owned by Haider rather than the provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CacheRewarmReasonV1 {
    PlannedCompaction,
    ConfigurationChange,
    #[serde(other)]
    Unknown,
}

/// Response-local classification for an observed zero cache read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "class", rename_all = "snake_case")]
#[non_exhaustive]
pub enum CacheMissClassificationV1 {
    PrefixChanged {
        first: CacheBreakpointV1,
    },
    ControlNotEmitted {
        reason: CacheControlOmissionReasonV1,
    },
    BelowMinimum,
    Expired,
    PlannedCompaction,
    ConfigurationChange,
    SamePrefixInTtl,
    Unavailable,
    #[serde(other)]
    Unknown,
}

/// Hashes-and-counts-only evidence needed to explain one cache result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheRequestDiagnosticV1 {
    pub history_message_count: u64,
    pub stable_prefix_tokens: u64,
    pub breakpoint_hashes: CacheBreakpointHashesV1,
    /// Keyed identity of provider/model/account/auth/reasoning/cache epoch.
    /// Optional for additive decoding of records written before this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_domain_hash: Option<String>,
    /// `false` is an observed same-domain comparison, not an unavailable
    /// measurement. Absent means there was no comparable prior record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_domain_changed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_breakpoint: Option<PreviousCacheBreakpointV1>,
    pub prefix_match: CachePrefixMatchV1,
    pub control: CacheControlObservationV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cacheable_minimum_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reuse_gap_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rewarm: Option<CacheRewarmReasonV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classification: Option<CacheMissClassificationV1>,
}

/// Honest availability for a provider-reported cache counter. A reported
/// numeric zero is [`Self::Present`]; an omitted, malformed, or unsupported
/// counter is [`Self::Unavailable`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheStatAvailability {
    Present,
    #[default]
    Unavailable,
}

/// How a provider's reasoning-token detail relates to billed output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningAccounting {
    /// The detail is already included in `billed_output` (OpenAI-style).
    SubsetOfOutput,
    /// The detail is billed in addition to `billed_output`.
    AdditionalToOutput,
    #[default]
    Unavailable,
}

/// Provider-neutral token accounting for one cumulative usage snapshot.
///
/// `uncached_input` includes cache writes exactly once. Pricing derives
/// fresh non-write input as `uncached_input - cache_write_input`, then uses
/// the model registry's write and read rates.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NormalizedUsage {
    pub logical_input: u64,
    pub uncached_input: u64,
    pub cache_read_input: u64,
    pub cache_write_input: u64,
    #[serde(default)]
    pub cache_write_5m_input: u64,
    #[serde(default)]
    pub cache_write_1h_input: u64,
    pub billed_output: u64,
    #[serde(default)]
    pub reasoning_detail: u64,
    #[serde(default)]
    pub reasoning_accounting: ReasoningAccounting,
    #[serde(default)]
    pub cache_status: CacheStatAvailability,
    #[serde(default)]
    pub cache_write_status: CacheStatAvailability,
    #[serde(default)]
    pub cache_write_ttl_status: CacheStatAvailability,
    /// Logical input for which the read/uncached split is authoritative.
    /// This equals `logical_input` for a fully covered provider snapshot and
    /// zero for an unavailable one; cumulative folds sum it for coverage.
    #[serde(default)]
    pub cache_telemetry_input: u64,
    /// Reserved for explicit-cache resources whose providers bill storage.
    /// Current request adapters do not synthesize this from latency or
    /// resource metadata, so it remains absent until exact telemetry exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explicit_cache_storage_token_hours: Option<f64>,
}

/// Request lane for cache/cost aggregation. Variants are append-only.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum UsageRequestKind {
    #[default]
    MainTurn,
    Compaction,
    DelegatedAgent,
    #[serde(other)]
    Unknown,
}

/// Non-secret hashes of provider-visible prefix components. These are
/// instrumentation only and never enter provider request payloads.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrefixDigests {
    pub system: String,
    pub tools: String,
    pub immutable_history: String,
    pub model: String,
    pub auth_mode: String,
    pub reasoning_settings: String,
}

/// Coordinates of one usage snapshot's cache domain and cumulative lane.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageLaneDimensions {
    /// Exact adapter family selected by the provider implementation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_family: Option<String>,
    /// Exact selected effort. Absent means default, unknown, or inapplicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Exact selected speed tier. Absent means unknown or inapplicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<String>,
}

/// Coordinates of one usage snapshot's cache domain and cumulative lane.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageScope {
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_scope: Option<CredentialAlias>,
    pub auth_scope: String,
    /// Non-secret adapter-owned dimensions captured with this exact request.
    /// Old journal facts omit them, which backfill preserves as unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_family: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<String>,
    pub cache_epoch: String,
    /// Exact compiler estimate for the stable prefix behind this epoch.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub stable_prefix_tokens: u64,
    /// Daemon-owned component identities used to name otherwise mysterious
    /// cold boundaries on a later turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_boundaries: Option<CacheBoundaryIdentity>,
    #[serde(default)]
    pub request_kind: UsageRequestKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix_digests: Option<PrefixDigests>,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheBoundaryIdentity {
    pub instructions: String,
    pub tool_pack: String,
    pub system_version: String,
    pub web_tools: String,
    pub reasoning_settings: String,
}

/// Input-only model-registry estimate used by `/usage`. Output is excluded
/// because caching changes only input cost.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct CacheCostEstimate {
    pub input_with_cache_usd: f64,
    pub input_without_cache_usd: f64,
    pub estimated_savings_usd: f64,
    #[serde(default)]
    pub explicit_storage_usd: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageSource {
    ProviderReported,
    LocallyExact,
    Estimated,
}

/// What an adapter supports — requested features resolve explicitly (§4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilityDoc {
    pub provider: String,
    pub parallel_tools: FeatureResolve,
    pub streaming_tool_args: FeatureResolve,
    pub vision: FeatureResolve,
    /// Native document blocks versus daemon-emulated extracted text.
    #[serde(default)]
    pub pdf_documents: FeatureResolve,
    pub thinking_visible: FeatureResolve,
    pub context_limit: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeatureResolve {
    Native,
    ExplicitlyEmulated,
    #[default]
    Unsupported,
}
